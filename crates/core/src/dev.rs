//! `lof dev` run locally: the free port the command listens on and the `--port` flag the
//! frameworks that ignore `PORT` need.
//!
//! Pure: the port is chosen over a predicate the CLI hands in (whether it
//! can bind a port), and the framework is read through
//! [`SourceTree`].

use std::ops::RangeInclusive;

use crate::detect::SourceTree;

/// The ports `lof dev` chooses from (`PORT`), in the order they are tried.
pub const DEV_PORTS: RangeInclusive<u16> = 4000..=4999;

/// The first port of [`DEV_PORTS`] for which `is_free` holds, asked in
/// order; `None` when every one is busy.
#[must_use]
pub fn free_port(mut is_free: impl FnMut(u16) -> bool) -> Option<u16> {
    DEV_PORTS.into_iter().find(|port| is_free(*port))
}

/// The packages that name a [`Framework`], in the order they are looked
/// for: the frameworks built on Vite come before Vite, and Vite before
/// Hono (a Vite project serving Hono through a plugin runs Vite).
const FRAMEWORKS: [(&str, Framework); 6] = [
    ("next", Framework::Next),
    ("astro", Framework::Astro),
    ("@sveltejs/kit", Framework::SvelteKit),
    ("expo", Framework::Expo),
    ("vite", Framework::Vite),
    ("hono", Framework::Hono),
];

/// A JavaScript framework `lof dev` knows, by the dependencies of
/// `package.json` (the table of [`StackDetector`](crate::detect::StackDetector)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    /// `next`.
    Next,
    /// `astro`.
    Astro,
    /// `@sveltejs/kit`.
    SvelteKit,
    /// `vite`.
    Vite,
    /// `hono`.
    Hono,
    /// `expo`.
    Expo,
}

impl Framework {
    /// The framework of the repository in `tree`; `None` without a
    /// `package.json` (or one that is not a JSON object) or when its
    /// `dependencies` and `devDependencies` list none of them. `SvelteKit`
    /// and Astro win over Vite, which they depend on.
    #[must_use]
    pub fn of(tree: &impl SourceTree) -> Option<Self> {
        let text = tree.read("package.json")?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let json = json.as_object()?;
        let depends_on = |package: &str| {
            ["dependencies", "devDependencies"].iter().any(|table| {
                json.get(*table)
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|table| table.contains_key(package))
            })
        };
        FRAMEWORKS
            .iter()
            .find(|(package, _)| depends_on(package))
            .map(|(_, framework)| *framework)
    }

    /// Whether the framework's dev server ignores `PORT` and takes
    /// `--port N` instead (Vite, Astro; Expo; `SvelteKit`, whose dev server is Vite's). Next and a Hono
    /// server (`Bun.serve`, `@hono/node-server` by the template) read
    /// `PORT`.
    #[must_use]
    pub const fn ignores_port(self) -> bool {
        match self {
            Self::Vite | Self::Astro | Self::SvelteKit | Self::Expo => true,
            Self::Next | Self::Hono => false,
        }
    }
}

/// The port `command` names with `--port N` or `--port=N`.
///
/// It is the port its server listens on, which `lof dev` then takes for
/// `PORT` and the URL it prints; the last one when it names several;
/// `None` when it names none or the word after `--port` is not a port.
#[must_use]
pub fn named_port(command: &str) -> Option<u16> {
    let mut named = None;
    let mut words = command.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let value = if word == "--port" {
            words.peek().copied()
        } else {
            word.strip_prefix("--port=")
        };
        if let Some(value) = value {
            named = value.parse().ok();
        }
    }
    named
}

/// `command` as `lof dev` runs it on `port`: with `--port <port>`
/// appended when `framework` ignores `PORT` (after `--` for `npm`, which
/// keeps the flags before it for itself), else unchanged.
///
/// A command that already names `--port` is left as written: its port is
/// the one `lof dev` runs it on ([`named_port`]).
#[must_use]
pub fn with_port(command: &str, framework: Option<Framework>, port: u16) -> String {
    let command = command.trim_end();
    let words: Vec<&str> = command.split_whitespace().collect();
    let named = words
        .iter()
        .any(|word| *word == "--port" || word.starts_with("--port="));
    if named || !framework.is_some_and(Framework::ignores_port) {
        return command.to_owned();
    }
    let npm = words.first() == Some(&"npm") && !words.contains(&"--");
    let separator = if npm { " --" } else { "" };
    format!("{command}{separator} --port {port}")
}

/// The name of a user's tunnel environment in `project`.
///
/// `<project>-dev` in single-user mode; with the `tenants` module,
/// `<project>-<login>` of `tenant_login`, lowercased, every character
/// that is no ASCII letter or digit a `-`. The daemon names the tunnel it
/// creates so, and `lof dev --share` the one it shares before the
/// upgrade. The caller checks that it is an environment name.
#[must_use]
pub fn tunnel_name(project: &str, tenant_login: Option<&str>) -> String {
    let suffix = tenant_login.map_or_else(
        || "dev".to_owned(),
        |login| {
            login
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect()
        },
    );
    format!("{project}-{suffix}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A tree of in-memory files.
    struct Files(BTreeMap<&'static str, &'static str>);

    impl Files {
        fn of(files: &[(&'static str, &'static str)]) -> Self {
            Self(files.iter().copied().collect())
        }
    }

    impl SourceTree for Files {
        fn read(&self, path: &str) -> Option<String> {
            self.0.get(path).map(|text| (*text).to_owned())
        }

        fn exists(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }

        fn is_dir(&self, path: &str) -> bool {
            let prefix = format!("{path}/");
            self.0.keys().any(|file| file.starts_with(&prefix))
        }
    }

    /// The framework of a `package.json` holding `text`.
    fn framework_of(text: &'static str) -> Option<Framework> {
        Framework::of(&Files::of(&[("package.json", text)]))
    }

    mod free_port {
        use super::*;

        // TC: DEV-01
        #[test]
        fn picks_4000_when_every_port_is_free() {
            assert_eq!(free_port(|_| true), Some(4000));
        }

        // TC: DEV-01
        #[test]
        fn skips_port_when_it_is_busy() {
            assert_eq!(free_port(|port| port != 4000), Some(4001));
        }

        // TC: DEV-01
        #[test]
        fn skips_every_busy_port_when_several_are_busy() {
            assert_eq!(free_port(|port| port > 4010), Some(4011));
        }

        // TC: DEV-01
        #[test]
        fn picks_nothing_when_every_port_is_busy() {
            assert_eq!(free_port(|_| false), None);
        }

        // TC: DEV-01
        #[test]
        fn asks_only_ports_4000_to_4999_in_order_when_every_port_is_busy() {
            let mut asked = Vec::new();
            let _none = free_port(|port| {
                asked.push(port);
                false
            });
            assert_eq!(asked, (4000..=4999).collect::<Vec<u16>>());
        }
    }

    mod framework {
        use super::*;

        // TC: DEV-02
        #[test]
        fn is_vite_when_package_json_lists_vite() {
            assert_eq!(
                framework_of(r#"{"devDependencies": {"vite": "^5.4.0"}}"#),
                Some(Framework::Vite)
            );
        }

        // TC: DEV-02
        #[test]
        fn is_hono_when_package_json_lists_hono() {
            assert_eq!(
                framework_of(r#"{"dependencies": {"hono": "^4.6.0"}}"#),
                Some(Framework::Hono)
            );
        }

        // TC: DEV-02
        #[test]
        fn is_astro_when_package_json_lists_astro_and_vite() {
            assert_eq!(
                framework_of(r#"{"dependencies": {"astro": "^4.16.0", "vite": "^5.4.0"}}"#),
                Some(Framework::Astro)
            );
        }

        // TC: DEV-02
        #[test]
        fn is_expo_when_package_json_lists_expo() {
            assert_eq!(
                framework_of(r#"{"dependencies": {"expo": "^51.0.0"}}"#),
                Some(Framework::Expo)
            );
        }

        // TC: DEV-02
        #[test]
        fn is_sveltekit_when_package_json_lists_kit_and_vite() {
            assert_eq!(
                framework_of(
                    r#"{"devDependencies": {"@sveltejs/kit": "^2.0.0", "vite": "^5.4.0"}}"#
                ),
                Some(Framework::SvelteKit)
            );
        }

        // TC: DEV-02
        #[test]
        fn is_none_when_there_is_no_package_json() {
            assert_eq!(Framework::of(&Files::of(&[("go.mod", "module x\n")])), None);
        }
    }

    mod ignores_port {
        use super::*;

        // TC: DEV-02
        #[test]
        fn holds_when_vite() {
            assert!(Framework::Vite.ignores_port());
        }

        // TC: DEV-02
        #[test]
        fn holds_when_astro() {
            assert!(Framework::Astro.ignores_port());
        }

        // TC: DEV-02 (Expo too)
        #[test]
        fn holds_when_expo() {
            assert!(Framework::Expo.ignores_port());
        }

        // TC: DEV-02 (its dev server is Vite's)
        #[test]
        fn holds_when_sveltekit() {
            assert!(Framework::SvelteKit.ignores_port());
        }

        // TC: DEV-02
        #[test]
        fn fails_when_next() {
            assert!(!Framework::Next.ignores_port());
        }

        // TC: DEV-02
        #[test]
        fn fails_when_hono() {
            assert!(!Framework::Hono.ignores_port());
        }
    }

    mod named_port {
        use super::*;

        // TC: DEV-02
        #[test]
        fn is_port_after_flag_when_command_names_port() {
            assert_eq!(named_port("vite --port 5000"), Some(5000));
        }

        // TC: DEV-02
        #[test]
        fn is_port_after_equals_when_command_names_port_with_equals() {
            assert_eq!(named_port("npm run dev -- --port=5000"), Some(5000));
        }

        // TC: DEV-02
        #[test]
        fn is_last_port_when_command_names_several() {
            assert_eq!(named_port("vite --port 5000 --port 5001"), Some(5001));
        }

        // TC: DEV-02
        #[test]
        fn is_none_when_command_names_no_port() {
            assert_eq!(named_port("bun dev"), None);
        }

        // TC: DEV-02
        #[test]
        fn is_none_when_word_after_flag_is_not_port() {
            assert_eq!(named_port("vite --port $PORT"), None);
        }
    }

    mod with_port {
        use super::*;

        // TC: DEV-02
        #[test]
        fn appends_port_flag_when_vite() {
            assert_eq!(
                with_port("bun dev", Some(Framework::Vite), 4001),
                "bun dev --port 4001"
            );
        }

        // TC: DEV-02 (npm passes a script's flags only after `--`)
        #[test]
        fn appends_port_flag_after_double_dash_when_npm_runs_vite() {
            assert_eq!(
                with_port("npm run dev", Some(Framework::Vite), 4001),
                "npm run dev -- --port 4001"
            );
        }

        // TC: DEV-02
        #[test]
        fn appends_port_flag_without_second_double_dash_when_npm_has_one() {
            assert_eq!(
                with_port("npm run dev -- --host", Some(Framework::Vite), 4001),
                "npm run dev -- --host --port 4001"
            );
        }

        // TC: DEV-02 (its port is `named_port`'s: `PORT` and the URL follow it)
        #[test]
        fn keeps_command_when_it_names_port() {
            assert_eq!(
                with_port("vite --port 5000", Some(Framework::Vite), 4001),
                "vite --port 5000"
            );
        }

        // TC: DEV-02
        #[test]
        fn keeps_command_when_hono() {
            assert_eq!(with_port("bun dev", Some(Framework::Hono), 4001), "bun dev");
        }

        // TC: DEV-02
        #[test]
        fn keeps_command_when_no_framework_is_known() {
            assert_eq!(with_port("go run .", None, 4001), "go run .");
        }
    }

    mod tunnel_name {
        use super::*;

        // TC: DEVT-01
        #[test]
        fn is_project_dash_dev_when_single_user() {
            assert_eq!(tunnel_name("web", None), "web-dev");
        }

        // TC: DEVT-02
        #[test]
        fn is_project_dash_login_when_tenants() {
            assert_eq!(tunnel_name("web", Some("Ann.Lee")), "web-ann-lee");
        }
    }
}
