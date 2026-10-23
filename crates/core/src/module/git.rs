//! [`ModuleGit`]: the core's [`Git`] port served by the `git.*` modules.

use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::hooks::ModuleCalls;
use super::registry::ModuleRegistry;
use crate::domain::{
    DeviceCode, GitProvider, Host, Login, ModuleName, RepoRef, SecretKey, SecretValue,
};
use crate::error::{Error, System};
use crate::ports::{
    DeviceGrant, DeviceStart, Git, GitAccess, NewRepo, PackageRef, RepoRole, Template,
};

/// The [`Git`] port answered by the forge modules of a [`ModuleRegistry`]
/// through `M` (the module host's `ModuleSet` in the daemon).
///
/// A call goes to the module `git.<provider>` of the connection, and only
/// while its manifest's `git_provider.hosts` names the connection's host
/// (the forge is found by the host of `git remote`); a host it
/// does not serve is [`Error::Validation`] (GITF-06). A module of any
/// other name never gets the call, so no module can collect another
/// forge's credentials by claiming its host. Each method is one `handle-tool` call of that module
/// without an actor, the connection's plaintext credential in its
/// arguments for that call only (the service unsealed it; the module never
/// stores it). The tools are not declared in the manifest's `tools`, so
/// they are no MCP tools: only the core calls them.
///
/// | Method | Tool | Arguments | Answer |
/// |---|---|---|---|
/// | [`Git::account`] | `validate_token` | `{"host","token"}` | `{"login"}` |
/// | [`Git::repo_role`] | `repo_role` | `{"host","login","token","repo"}` | `{"role":"admin"\|"write"\|"read"\|"none"}` |
/// | [`Git::image_exists`] | `image_exists` | `{"host","login","token","owner","name","tag"}` | `{"exists"}` |
/// | [`Git::put_ci_secret`] | `put_ci_secret` | `{"host","login","token","repo","key","value"}` | `{}` |
/// | [`Git::list_templates`] | `list_templates` | `{"host","login","token","repos":["owner/name",…]}` | `{"templates":[{"repo","manifest"}]}` (`manifest` null without `liftoff.toml`) |
/// | [`Git::generate_from_template`] | `generate_from_template` | `{"host","login","token","template","owner","name","private"}` | `{"repo"}` |
/// | [`Git::device_start`] | `device_start` | `{"host"}` | `{"device_code","user_code","verification_uri","interval","expires_in"}` |
/// | [`Git::device_poll`] | `device_poll` | `{"host","device_code"}` | `{"status":"pending"}`, `{"status":"slow_down","interval"?}` or `{"status":"granted","token","expires_in"?}` (seconds the token stays valid) |
///
/// The device flow is asked of a module only while its manifest declares
/// `git_provider.oauth`; otherwise it is [`Error::Validation`] with a hint
/// to connect with a token (GIT-01).
///
/// The module's errors map by their code (`forbidden`, `validation`, …);
/// `upstream`, a forge that keeps failing (GITF-04), is
/// [`Error::Upstream`] of [`System::Forge`]. A `git.<provider>` that is
/// not installed is [`Error::ModuleRequired`] of it; one that does not
/// serve the host is [`Error::Validation`]; one that serves the host but
/// is disabled is [`Error::ModuleRequired`] of it (MOD-09).
#[derive(Debug)]
pub struct ModuleGit<M> {
    registry: Arc<ModuleRegistry>,
    calls: M,
}

impl<M: ModuleCalls> ModuleGit<M> {
    /// The port over the forge modules of `registry`, called through
    /// `calls`.
    #[must_use]
    pub const fn new(registry: Arc<ModuleRegistry>, calls: M) -> Self {
        Self { registry, calls }
    }

    /// The registry it reads.
    #[must_use]
    pub const fn registry(&self) -> &Arc<ModuleRegistry> {
        &self.registry
    }

    /// The calls into the modules.
    #[must_use]
    pub const fn calls(&self) -> &M {
        &self.calls
    }
}

impl<M: ModuleCalls> ModuleGit<M> {
    /// The provider whose `git.<provider>` module serves `host`
    /// ([`Git::provider_of`]).
    fn provider_serving(&self, host: &Host) -> Result<GitProvider, Error> {
        // Only `git.<provider>` modules name a forge (GITF-06); the hosts
        // compare as written, both sides lower-cased at the boundary.
        let serving: Vec<(GitProvider, bool)> = self
            .registry
            .list()
            .into_iter()
            .filter(|(_, manifest, _)| {
                manifest
                    .git_provider
                    .as_ref()
                    .is_some_and(|spec| spec.hosts.iter().any(|served| served == host.as_str()))
            })
            .filter_map(|(name, _, status)| {
                let provider = name.as_str().strip_prefix("git.")?;
                GitProvider::parse(provider)
                    .ok()
                    .map(|provider| (provider, status.enabled))
            })
            .collect();
        match serving.as_slice() {
            [] => Err(Error::invalid(
                "host",
                format!("no git module serves {host}: add it to the hosts of a git.* module"),
            )),
            [(provider, true)] => Ok(*provider),
            [(provider, false)] => {
                Err(Error::module_required(format!("git.{}", provider.as_str())))
            }
            _ => Err(Error::invalid(
                "host",
                format!("several git modules serve {host}: keep it in the hosts of one"),
            )),
        }
    }

    /// The module that serves `host` for `provider` (GITF-06): only
    /// `git.<provider>`, and only while its `git_provider.hosts` names
    /// `host`. A module of another name claiming the host never gets the
    /// call (nor the connection's token), whatever the install order.
    fn serving(&self, provider: GitProvider, host: &Host) -> Result<ModuleName, Error> {
        let expected = format!("git.{}", provider.as_str());
        let module = ModuleName::new(&expected)
            .map_err(|_invalid| Error::internal_msg(format!("{expected} is not a module name")))?;
        let Some((_name, manifest, status)) = self
            .registry
            .list()
            .into_iter()
            .find(|(name, _, _)| name == &module)
        else {
            return Err(Error::module_required(expected));
        };
        // Both sides are lower-cased at the boundary (`Host::new`, the
        // manifest's `git_provider.hosts`), so the hosts compare as written.
        let serves = manifest
            .git_provider
            .as_ref()
            .is_some_and(|spec| spec.hosts.iter().any(|served| served == host.as_str()));
        if !serves {
            return Err(Error::invalid(
                "host",
                format!(
                    "no git module serves {host}: add it to the hosts of the module {expected}"
                ),
            ));
        }
        if !status.enabled {
            return Err(Error::module_required(expected));
        }
        Ok(module)
    }

    /// Refuses the device flow of a forge whose module declares no OAuth
    /// application (`git_provider.oauth`), with a hint to connect with a
    /// token instead (GIT-01); the module is not asked.
    fn oauth(&self, provider: GitProvider, host: &Host) -> Result<(), Error> {
        let module = self.serving(provider, host)?;
        let oauth = self
            .registry
            .manifest(&module)
            .and_then(|manifest| manifest.git_provider)
            .is_some_and(|spec| spec.oauth);
        if oauth {
            return Ok(());
        }
        Err(Error::invalid(
            "provider",
            format!(
                "the module {module} has no OAuth application (its config client_id): connect with a token instead, `lof git connect {provider} --host {host} --token <token>`",
                provider = provider.as_str()
            ),
        ))
    }

    /// The tool `tool` of the module serving `host`, with `args`: its
    /// answer as `T`.
    async fn call<T: DeserializeOwned>(
        &self,
        provider: GitProvider,
        host: &Host,
        tool: &str,
        args: Value,
    ) -> Result<T, Error> {
        let module = self.serving(provider, host)?;
        let answer = self
            .calls
            .handle_tool(&module, None, tool, &args.to_string())
            .await
            .map_err(|error| match error {
                Error::Upstream {
                    system: System::Module,
                    detail,
                } => Error::upstream(System::Forge, detail),
                other => other,
            })?;
        serde_json::from_str(&answer).map_err(|error| {
            tracing::error!(%module, tool, %error, "a forge module answered a tool call it does not speak");
            Error::internal_msg(format!("module {module} answered {tool} with {error}"))
        })
    }

    /// [`ModuleGit::call`] as `connection`: its host, login and token
    /// come first in the arguments.
    async fn call_as<T: DeserializeOwned>(
        &self,
        connection: &GitAccess,
        tool: &str,
        args: Value,
    ) -> Result<T, Error> {
        let mut fields = json!({
            "host": connection.host.as_str(),
            "login": connection.login.as_str(),
            "token": connection.token.expose(),
        });
        if let (Some(fields), Value::Object(args)) = (fields.as_object_mut(), args) {
            fields.extend(args);
        }
        self.call(connection.provider, &connection.host, tool, fields)
            .await
    }
}

/// `validate_token`'s answer.
#[derive(Deserialize)]
struct Account {
    login: String,
}

/// `repo_role`'s answer.
#[derive(Deserialize)]
struct Role {
    role: String,
}

/// `image_exists`'s answer.
#[derive(Deserialize)]
struct Exists {
    exists: bool,
}

/// `list_templates`'s answer.
#[derive(Deserialize)]
struct Templates {
    templates: Vec<TemplateAnswer>,
}

/// One template of [`Templates`].
#[derive(Deserialize)]
struct TemplateAnswer {
    repo: String,
    manifest: Option<String>,
}

/// `device_start`'s answer.
#[derive(Deserialize)]
struct Started {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

/// `device_poll`'s answer.
#[derive(Deserialize)]
struct Poll {
    status: String,
    token: Option<String>,
    /// With `slow_down`: the interval the forge asks for.
    interval: Option<u64>,
    /// With `granted`: seconds the token stays valid, if the forge said.
    expires_in: Option<u64>,
}

/// `generate_from_template`'s answer.
#[derive(Deserialize)]
struct Generated {
    repo: String,
}

/// A repository a forge named, as [`RepoRef`].
fn forge_repo(raw: &str) -> Result<RepoRef, Error> {
    RepoRef::new(raw).map_err(|_invalid| {
        Error::upstream(
            System::Forge,
            format!("the forge named the repository {raw:?}"),
        )
    })
}

impl<M: ModuleCalls> Git for ModuleGit<M> {
    fn provider_of(&self, host: &Host) -> impl Future<Output = Result<GitProvider, Error>> + Send {
        core::future::ready(self.provider_serving(host))
    }

    async fn account(
        &self,
        provider: GitProvider,
        host: &Host,
        token: &SecretValue,
    ) -> Result<Login, Error> {
        let args = json!({ "host": host.as_str(), "token": token.expose() });
        let account: Account = self.call(provider, host, "validate_token", args).await?;
        Login::new(&account.login).map_err(|_invalid| {
            Error::upstream(
                System::Forge,
                format!("{host}: the account has no valid login"),
            )
        })
    }

    async fn repo_role(&self, connection: &GitAccess, repo: &RepoRef) -> Result<RepoRole, Error> {
        let args = json!({ "repo": repo.to_string() });
        let answer: Role = self.call_as(connection, "repo_role", args).await?;
        match answer.role.as_str() {
            "admin" => Ok(RepoRole::Admin),
            "write" => Ok(RepoRole::Write),
            "read" => Ok(RepoRole::Read),
            "none" => Ok(RepoRole::None),
            other => {
                tracing::error!(role = other, "a forge module answered an unknown role");
                Err(Error::internal_msg(format!(
                    "a forge module answered the role {other:?}"
                )))
            }
        }
    }

    async fn image_exists(
        &self,
        connection: &GitAccess,
        image: &PackageRef,
    ) -> Result<bool, Error> {
        let args = json!({
            "owner": image.owner,
            "name": image.name,
            "tag": image.tag.as_str(),
        });
        let answer: Exists = self.call_as(connection, "image_exists", args).await?;
        Ok(answer.exists)
    }

    async fn put_ci_secret(
        &self,
        connection: &GitAccess,
        repo: &RepoRef,
        key: &SecretKey,
        value: &SecretValue,
    ) -> Result<(), Error> {
        let args = json!({
            "repo": repo.to_string(),
            "key": key.as_str(),
            "value": value.expose(),
        });
        // The answer is `{}`; only its success counts.
        let _answer: Value = self.call_as(connection, "put_ci_secret", args).await?;
        Ok(())
    }

    async fn list_templates(
        &self,
        connection: &GitAccess,
        repos: &[RepoRef],
    ) -> Result<Vec<Template>, Error> {
        let names: Vec<String> = repos.iter().map(ToString::to_string).collect();
        let answer: Templates = self
            .call_as(connection, "list_templates", json!({ "repos": names }))
            .await?;
        answer
            .templates
            .into_iter()
            .map(|template| {
                Ok(Template {
                    repo: forge_repo(&template.repo)?,
                    manifest: template.manifest,
                })
            })
            .collect()
    }

    async fn generate_from_template(
        &self,
        connection: &GitAccess,
        template: &RepoRef,
        target: &NewRepo,
    ) -> Result<RepoRef, Error> {
        let args = json!({
            "template": template.to_string(),
            "owner": target.repo.owner(),
            "name": target.repo.name(),
            "private": target.private,
        });
        let answer: Generated = self
            .call_as(connection, "generate_from_template", args)
            .await?;
        forge_repo(&answer.repo)
    }

    async fn device_start(&self, provider: GitProvider, host: &Host) -> Result<DeviceStart, Error> {
        self.oauth(provider, host)?;
        let answer: Started = self
            .call(
                provider,
                host,
                "device_start",
                json!({ "host": host.as_str() }),
            )
            .await?;
        let device_code = DeviceCode::new(&answer.device_code).map_err(|invalid| {
            tracing::error!(%invalid, "a forge module answered an invalid device code");
            Error::internal_msg("a forge module answered an invalid device code")
        })?;
        Ok(DeviceStart {
            device_code,
            user_code: answer.user_code,
            verification_uri: answer.verification_uri,
            interval: answer.interval,
            expires_in: answer.expires_in,
        })
    }

    async fn device_poll(
        &self,
        provider: GitProvider,
        host: &Host,
        device_code: &DeviceCode,
    ) -> Result<DeviceGrant, Error> {
        self.oauth(provider, host)?;
        let args = json!({ "host": host.as_str(), "device_code": device_code.expose() });
        let answer: Poll = self.call(provider, host, "device_poll", args).await?;
        match (answer.status.as_str(), answer.token) {
            ("pending", _) => Ok(DeviceGrant::Pending),
            ("slow_down", _) => Ok(DeviceGrant::SlowDown {
                interval: answer.interval,
            }),
            ("granted", Some(token)) => Ok(DeviceGrant::Granted {
                token: SecretValue::new(token),
                expires_in: answer.expires_in,
            }),
            (status, _) => {
                tracing::error!(status, "a forge module answered an unknown device poll");
                Err(Error::internal_msg(format!(
                    "a forge module answered the device poll {status:?}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ImageTag;
    use crate::fakes::{FakeModules, ModuleCall};
    use crate::module::ModuleManifest;

    const FORGE: &str = "git.example.org";

    fn name(raw: &str) -> ModuleName {
        ModuleName::new(raw).unwrap()
    }

    /// The manifest of a forge module `module` serving `hosts`.
    fn forge_manifest(module: &str, hosts: &[&str]) -> ModuleManifest {
        ModuleManifest::parse(
            &json!({
                "name": module,
                "version": "0.1.0",
                "git_provider": { "hosts": hosts, "oauth": false },
            })
            .to_string(),
        )
        .unwrap()
    }

    /// The port over `git.forgejo` serving [`FORGE`], and the fake modules.
    fn forgejo() -> (ModuleGit<FakeModules>, FakeModules) {
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(name("git.forgejo"), forge_manifest("git.forgejo", &[FORGE]));
        let fake = FakeModules::new();
        (ModuleGit::new(registry, fake.clone()), fake)
    }

    fn connection(provider: GitProvider, host: &str) -> GitAccess {
        GitAccess {
            provider,
            host: Host::new(host).unwrap(),
            login: Login::new("carol").unwrap(),
            token: SecretValue::new("carol-token".to_owned()),
        }
    }

    fn repo() -> RepoRef {
        RepoRef::new("acme/web").unwrap()
    }

    /// The tool calls the fake received: module, actor, name, arguments.
    fn tool_calls(
        fake: &FakeModules,
    ) -> Vec<(String, Option<crate::domain::UserId>, String, Value)> {
        fake.calls()
            .into_iter()
            .filter_map(|call| match call {
                ModuleCall::Tool {
                    module,
                    actor,
                    name,
                    args,
                } => Some((module, actor, name, serde_json::from_str(&args).unwrap())),
                _ => None,
            })
            .collect()
    }

    mod dispatch {
        use super::*;

        // TC: GITF-01 (one `handle-tool` without an actor, the
        // connection's credential in its arguments)
        #[tokio::test]
        async fn calls_tool_with_connection_without_actor_when_asked_for_role() {
            let (git, fake) = forgejo();
            fake.tool(
                "git.forgejo",
                "repo_role",
                Ok(r#"{"role":"write"}"#.to_owned()),
            );
            let role = git
                .repo_role(&connection(GitProvider::Forgejo, FORGE), &repo())
                .await
                .unwrap();
            assert_eq!(
                (role, tool_calls(&fake)),
                (
                    RepoRole::Write,
                    vec![(
                        "git.forgejo".to_owned(),
                        None,
                        "repo_role".to_owned(),
                        json!({"host": FORGE, "login": "carol", "token": "carol-token", "repo": "acme/web"}),
                    )]
                )
            );
        }

        // TC: GITF-06 (the host is matched without regard to case)
        #[tokio::test]
        async fn calls_module_when_host_differs_only_in_case() {
            let registry = Arc::new(ModuleRegistry::new());
            registry.register(
                name("git.forgejo"),
                forge_manifest("git.forgejo", &["Git.Example.org"]),
            );
            let fake = FakeModules::new();
            fake.tool(
                "git.forgejo",
                "image_exists",
                Ok(r#"{"exists":true}"#.to_owned()),
            );
            let git = ModuleGit::new(registry, fake);
            let image = PackageRef {
                owner: "acme".to_owned(),
                name: "web".to_owned(),
                tag: ImageTag::new("sha-1").unwrap(),
            };
            assert!(
                git.image_exists(&connection(GitProvider::Forgejo, FORGE), &image)
                    .await
                    .unwrap()
            );
        }

        // TC: GITF-06
        #[tokio::test]
        async fn validation_without_call_when_installed_module_does_not_serve_host() {
            let (git, fake) = forgejo();
            let err = git
                .repo_role(
                    &connection(GitProvider::Forgejo, "code.example.org"),
                    &repo(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::Validation(_)) && fake.calls().is_empty(),
                "{err:?} {:?}",
                fake.calls()
            );
        }

        // TC: GITF-06, REST-10 (a forge whose module is not installed)
        #[tokio::test]
        async fn module_required_of_provider_when_no_module_of_it_is_installed() {
            let (git, fake) = forgejo();
            let err = git
                .account(
                    GitProvider::Github,
                    &Host::new("github.com").unwrap(),
                    &SecretValue::new("tok".to_owned()),
                )
                .await
                .unwrap_err();
            assert_eq!(
                (err.to_string(), fake.calls()),
                ("install the module `git.github`".to_owned(), Vec::new())
            );
        }

        /// A registry of `modules` (name, served hosts) in install order.
        fn registry_of(modules: &[(&str, &[&str])]) -> Arc<ModuleRegistry> {
            let registry = Arc::new(ModuleRegistry::new());
            for (module, hosts) in modules {
                registry.register(name(module), forge_manifest(module, hosts));
            }
            registry
        }

        // TC: GITF-06 (only `git.<provider>` gets the connection's token: a
        // module of another name claiming the host is never called)
        #[tokio::test]
        async fn module_required_without_call_when_only_other_module_claims_host() {
            let fake = FakeModules::new();
            fake.tool("foo.bar", "repo_role", Ok(r#"{"role":"admin"}"#.to_owned()));
            let git = ModuleGit::new(registry_of(&[("foo.bar", &[FORGE])]), fake.clone());
            let err = git
                .repo_role(&connection(GitProvider::Forgejo, FORGE), &repo())
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::ModuleRequired(module) if module == "git.forgejo")
                    && fake.calls().is_empty(),
                "{err:?} {:?}",
                fake.calls()
            );
        }

        // TC: GITF-06 (two modules claim the host: the connection's provider
        // picks, whatever the install order)
        #[tokio::test]
        async fn calls_module_of_provider_when_two_modules_claim_host() {
            let fake = FakeModules::new();
            for module in ["foo.bar", "git.github", "git.forgejo"] {
                fake.tool(module, "repo_role", Ok(r#"{"role":"read"}"#.to_owned()));
            }
            let registry = registry_of(&[
                ("foo.bar", &[FORGE]),
                ("git.github", &[FORGE]),
                ("git.forgejo", &[FORGE]),
            ]);
            let git = ModuleGit::new(registry, fake.clone());
            let _role = git
                .repo_role(&connection(GitProvider::Forgejo, FORGE), &repo())
                .await
                .unwrap();
            let called: Vec<String> = tool_calls(&fake)
                .into_iter()
                .map(|(module, ..)| module)
                .collect();
            assert_eq!(called, vec!["git.forgejo".to_owned()]);
        }

        // TC: GITF-06, MOD-09 (the module serving the host is disabled)
        #[tokio::test]
        async fn module_required_without_call_when_serving_module_is_disabled() {
            let (git, fake) = forgejo();
            assert!(git.registry().disable(&name("git.forgejo"), "trapped"));
            let err = git
                .repo_role(&connection(GitProvider::Forgejo, FORGE), &repo())
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::ModuleRequired(module) if module == "git.forgejo")
                    && fake.calls().is_empty(),
                "{err:?}"
            );
        }
    }

    mod provider_of {
        use super::*;

        /// A registry of `modules` (name, served hosts) in install order.
        fn registry_of(modules: &[(&str, &[&str])]) -> Arc<ModuleRegistry> {
            let registry = Arc::new(ModuleRegistry::new());
            for (module, hosts) in modules {
                registry.register(name(module), forge_manifest(module, hosts));
            }
            registry
        }

        fn forge() -> Host {
            Host::new(FORGE).unwrap()
        }

        // TC: SNIP-01, GITF-06 (the provider of a host is that of the
        // `git.<provider>` module serving it; nothing is called)
        #[tokio::test]
        async fn provider_of_serving_module_when_one_serves_host() {
            let fake = FakeModules::new();
            let registry = registry_of(&[
                ("foo.bar", &[FORGE]),
                ("git.forgejo", &["code.example.org"]),
                ("git.gitlab", &[FORGE]),
            ]);
            let git = ModuleGit::new(registry, fake.clone());
            let provider = git.provider_of(&forge()).await.unwrap();
            assert_eq!((provider, fake.calls()), (GitProvider::Gitlab, Vec::new()));
        }

        // TC: GITF-06 (a module of another name claiming the host does not
        // make it a forge)
        #[tokio::test]
        async fn validation_on_host_when_no_git_module_serves_host() {
            let git = ModuleGit::new(
                registry_of(&[
                    ("foo.bar", &[FORGE]),
                    ("git.forgejo", &["code.example.org"]),
                ]),
                FakeModules::new(),
            );
            let err = git.provider_of(&forge()).await.unwrap_err();
            assert!(
                matches!(&err, Error::Validation(invalid) if invalid.field == "host"),
                "{err:?}"
            );
        }

        // TC: GITF-06 (two forges claim one host: no guess)
        #[tokio::test]
        async fn validation_on_host_when_two_providers_serve_host() {
            let git = ModuleGit::new(
                registry_of(&[("git.github", &[FORGE]), ("git.forgejo", &[FORGE])]),
                FakeModules::new(),
            );
            let err = git.provider_of(&forge()).await.unwrap_err();
            assert!(
                matches!(&err, Error::Validation(invalid) if invalid.field == "host"),
                "{err:?}"
            );
        }

        // TC: GITF-06, MOD-09 (the module serving the host is disabled)
        #[tokio::test]
        async fn module_required_when_serving_module_is_disabled() {
            let (git, _fake) = forgejo();
            assert!(git.registry().disable(&name("git.forgejo"), "trapped"));
            let err = git.provider_of(&forge()).await.unwrap_err();
            assert!(
                matches!(&err, Error::ModuleRequired(module) if module == "git.forgejo"),
                "{err:?}"
            );
        }
    }

    mod answers {
        use super::*;

        // TC: GITF-04 (the module's `upstream` is the forge's failure)
        #[tokio::test]
        async fn upstream_of_forge_when_module_answers_upstream() {
            let (git, fake) = forgejo();
            fake.tool(
                "git.forgejo",
                "repo_role",
                Err(Error::upstream(System::Module, "git.example.org: HTTP 503")),
            );
            let err = git
                .repo_role(&connection(GitProvider::Forgejo, FORGE), &repo())
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Upstream { system: System::Forge, detail } if detail.contains("503")),
                "{err:?}"
            );
        }

        // TC: GITF-05 (the module's own error passes through)
        #[tokio::test]
        async fn forbidden_when_module_answers_forbidden() {
            let (git, fake) = forgejo();
            fake.tool(
                "git.forgejo",
                "repo_role",
                Err(Error::forbidden("run `lof git connect` again")),
            );
            let err = git
                .repo_role(&connection(GitProvider::Forgejo, FORGE), &repo())
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
        }

        // TC: GITF-01 (a module that answers outside the contract fails
        // the call instead of granting anything)
        #[tokio::test]
        async fn internal_when_module_answers_unknown_role() {
            let (git, fake) = forgejo();
            fake.tool(
                "git.forgejo",
                "repo_role",
                Ok(r#"{"role":"owner"}"#.to_owned()),
            );
            let err = git
                .repo_role(&connection(GitProvider::Forgejo, FORGE), &repo())
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Internal(_)), "{err:?}");
        }

        // TC: REST-02 (the login the module answered, lower-cased)
        #[tokio::test]
        async fn login_of_answer_when_token_is_valid() {
            let (git, fake) = forgejo();
            fake.tool(
                "git.forgejo",
                "validate_token",
                Ok(r#"{"login":"Carol"}"#.to_owned()),
            );
            let login = git
                .account(
                    GitProvider::Forgejo,
                    &Host::new(FORGE).unwrap(),
                    &SecretValue::new("tok".to_owned()),
                )
                .await
                .unwrap();
            assert_eq!(
                (login.as_str(), tool_calls(&fake)[0].3.clone()),
                ("carol", json!({"host": FORGE, "token": "tok"}))
            );
        }
    }

    mod device {
        use super::*;

        const GITHUB: &str = "github.com";

        /// The port over `git.github` serving `github.com` with an OAuth
        /// application, and the fake modules.
        fn github() -> (ModuleGit<FakeModules>, FakeModules) {
            let registry = Arc::new(ModuleRegistry::new());
            let manifest = ModuleManifest::parse(
                &json!({
                    "name": "git.github",
                    "version": "0.1.0",
                    "git_provider": { "hosts": [GITHUB], "oauth": true },
                })
                .to_string(),
            )
            .unwrap();
            registry.register(name("git.github"), manifest);
            let fake = FakeModules::new();
            (ModuleGit::new(registry, fake.clone()), fake)
        }

        async fn poll(answer: &str) -> Result<DeviceGrant, Error> {
            let (git, fake) = github();
            fake.tool("git.github", "device_poll", Ok(answer.to_owned()));
            git.device_poll(
                GitProvider::Github,
                &Host::new(GITHUB).unwrap(),
                &DeviceCode::new("dc-1").unwrap(),
            )
            .await
        }

        // TC: GIT-01 (RFC 8628 3.5 `slow_down`, with the forge's interval)
        #[tokio::test]
        async fn slow_down_with_interval_when_module_answers_slow_down() {
            let grant = poll(r#"{"status":"slow_down","interval":10}"#)
                .await
                .unwrap();
            assert_eq!(grant, DeviceGrant::SlowDown { interval: Some(10) });
        }

        // TC: GIT-01 (the token's lifetime the module passes on)
        #[tokio::test]
        async fn granted_with_lifetime_when_module_names_expires_in() {
            let grant = poll(r#"{"status":"granted","token":"gho_1","expires_in":7200}"#)
                .await
                .unwrap();
            assert_eq!(
                grant,
                DeviceGrant::Granted {
                    token: SecretValue::new("gho_1".to_owned()),
                    expires_in: Some(7200),
                }
            );
        }

        // TC: GIT-01 (a forge that did not say how long: the CLI adds 5 s)
        #[tokio::test]
        async fn slow_down_without_interval_when_module_names_none() {
            let grant = poll(r#"{"status":"slow_down"}"#).await.unwrap();
            assert_eq!(grant, DeviceGrant::SlowDown { interval: None });
        }

        // TC: GIT-01 (the device code a module hands out is validated as
        // any input: an empty one fails the call)
        #[tokio::test]
        async fn internal_when_module_answers_empty_device_code() {
            let (git, fake) = github();
            fake.tool(
                "git.github",
                "device_start",
                Ok(json!({
                    "device_code": "", "user_code": "WDJB-MJHT",
                    "verification_uri": "https://github.com/login/device",
                    "interval": 5, "expires_in": 900,
                })
                .to_string()),
            );
            let err = git
                .device_start(GitProvider::Github, &Host::new(GITHUB).unwrap())
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Internal(_)), "{err:?}");
        }
    }
}
