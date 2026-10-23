//! Environment slices through `sudo -n systemctl`.

use liftoff_core::domain::{Limits, SliceName};
use liftoff_core::ports::Systemd;
use liftoff_core::{Error, System};

use crate::cmd::Cmd;

/// Runs `sudo -n systemctl` (non-interactive; the sudoers rule of the
/// `liftoff` user allows exactly `set-property --runtime` and `stop` on
/// `apps-lf-*`).
///
/// Limits are set with `--runtime`: the drop-ins go to
/// `/run/systemd/system.control`, so they never outlive a reboot; the reconciler re-applies them for running environments.
#[derive(Debug, Clone, Copy, Default)]
pub struct SudoSystemd;

impl SudoSystemd {
    /// A new adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn set_limits_cmd(slice: &SliceName, limits: Limits) -> Cmd {
        Cmd::new("sudo").args([
            "-n".to_owned(),
            "systemctl".to_owned(),
            "set-property".to_owned(),
            "--runtime".to_owned(),
            slice.as_str().to_owned(),
            format!("CPUQuota={}%", limits.cpus.hundredths()),
            format!("MemoryMax={}M", limits.memory_mb.get()),
            "MemorySwapMax=0".to_owned(),
        ])
    }

    fn stop_cmd(slice: &SliceName) -> Cmd {
        Cmd::new("sudo").args(["-n", "systemctl", "stop", slice.as_str()])
    }
}

impl Systemd for SudoSystemd {
    async fn set_slice_limits(&self, slice: &SliceName, limits: Limits) -> Result<(), Error> {
        Self::set_limits_cmd(slice, limits)
            .run_checked(System::Systemd)
            .await
            .map(drop)
    }

    async fn stop_slice(&self, slice: &SliceName) -> Result<(), Error> {
        Self::stop_cmd(slice)
            .run_checked(System::Systemd)
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use liftoff_core::domain::{Cpus, EnvName, MemoryMb, ProjectName};

    use super::*;

    fn slice(project: &str, env: &str) -> SliceName {
        let project = ProjectName::new(project).expect("valid project");
        let env = EnvName::new(env).expect("valid env");
        SliceName::new(&project, &env)
    }

    fn argv(cmd: &Cmd) -> Vec<String> {
        cmd.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    mod set_slice_limits {
        use super::*;

        // TC: v1 ADP-04
        #[test]
        fn quota_100_and_memory_1024_when_default_limits() {
            let cmd = SudoSystemd::set_limits_cmd(&slice("acme", "qa"), Limits::default());
            assert_eq!(cmd.program, "sudo");
            assert_eq!(
                argv(&cmd),
                [
                    "-n",
                    "systemctl",
                    "set-property",
                    "--runtime",
                    "apps-lf-acme-qa.slice",
                    "CPUQuota=100%",
                    "MemoryMax=1024M",
                    "MemorySwapMax=0"
                ]
            );
        }

        // TC: v1 ADP-04
        #[test]
        fn quota_50_when_cpus_is_half() {
            let limits = Limits {
                cpus: Cpus::parse("0.5").expect("valid cpus"),
                memory_mb: MemoryMb::new(512).expect("valid memory"),
            };
            let cmd = SudoSystemd::set_limits_cmd(&slice("acme", "qa"), limits);
            let argv = argv(&cmd);
            assert_eq!(argv.get(5).map(String::as_str), Some("CPUQuota=50%"));
            assert_eq!(argv.get(6).map(String::as_str), Some("MemoryMax=512M"));
        }

        // TC: v1 ADP-04
        #[test]
        fn slice_name_is_passed_escaped_when_names_contain_dashes() {
            let cmd = SudoSystemd::set_limits_cmd(&slice("my-co", "qa-2"), Limits::default());
            assert_eq!(
                argv(&cmd).get(4).map(String::as_str),
                Some(r"apps-lf-my\x2dco-qa\x2d2.slice")
            );
        }
    }

    mod stop_slice {
        use super::*;

        // TC: v1 ADP-04
        #[test]
        fn stops_the_slice_non_interactively() {
            let cmd = SudoSystemd::stop_cmd(&slice("acme", "qa"));
            assert_eq!(
                argv(&cmd),
                ["-n", "systemctl", "stop", "apps-lf-acme-qa.slice"]
            );
        }
    }

    /// `scripts/sudoers`, installed by `liftoffd setup`, admits exactly the
    /// commands this adapter runs.
    mod sudoers {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        use super::*;

        const SUDOERS: &str = include_str!("../../../scripts/sudoers");
        const PREFIX: &str = "liftoff ALL=(root) NOPASSWD: /usr/bin/systemctl ";

        /// The argument patterns of the rules (sudoers regular expressions,
        /// POSIX ERE over the arguments joined by spaces).
        fn patterns() -> Vec<&'static str> {
            SUDOERS
                .lines()
                .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
                .map(|line| {
                    line.strip_prefix(PREFIX)
                        .unwrap_or_else(|| panic!("not a systemctl rule of liftoff: {line}"))
                })
                .collect()
        }

        /// Whether a rule admits `systemctl <args>`, matched by `grep -E` as
        /// sudo matches it.
        fn admitted(args: &str) -> bool {
            patterns().into_iter().any(|pattern| {
                let mut grep = Command::new("grep")
                    .args(["-Eq", "-e", pattern])
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap();
                grep.stdin
                    .take()
                    .unwrap()
                    .write_all(format!("{args}\n").as_bytes())
                    .unwrap();
                grep.wait().unwrap().success()
            })
        }

        /// The arguments of `systemctl` in `cmd` (after `sudo -n systemctl`).
        fn systemctl_args(cmd: &Cmd) -> String {
            let argv = argv(cmd);
            assert_eq!(
                argv.get(..2),
                Some(&["-n".to_owned(), "systemctl".to_owned()][..])
            );
            argv[2..].join(" ")
        }

        // TC: SETUP-02
        #[test]
        fn has_one_rule_per_command_when_installed() {
            assert_eq!(
                patterns()
                    .iter()
                    .map(|pattern| (pattern.starts_with('^'), pattern.ends_with('$')))
                    .collect::<Vec<_>>(),
                vec![(true, true), (true, true)]
            );
        }

        // TC: SETUP-02
        #[test]
        fn admits_set_limits_when_the_limits_are_the_defaults() {
            let cmd = SudoSystemd::set_limits_cmd(&slice("acme", "qa"), Limits::default());
            assert!(admitted(&systemctl_args(&cmd)), "{:?}", argv(&cmd));
        }

        // TC: SETUP-02
        #[test]
        fn admits_set_limits_when_names_are_escaped_and_cpus_fractional() {
            let limits = Limits {
                cpus: Cpus::parse("0.25").expect("valid cpus"),
                memory_mb: MemoryMb::new(256).expect("valid memory"),
            };
            let cmd = SudoSystemd::set_limits_cmd(&slice("my-co", "qa-2"), limits);
            assert!(admitted(&systemctl_args(&cmd)), "{:?}", argv(&cmd));
        }

        // TC: SETUP-02
        #[test]
        fn admits_stop_when_the_unit_is_an_environment_slice() {
            let cmd = SudoSystemd::stop_cmd(&slice("acme", "qa"));
            assert!(admitted(&systemctl_args(&cmd)), "{:?}", argv(&cmd));
        }

        // TC: SETUP-02
        #[test]
        fn refuses_stop_when_another_unit_follows_the_slice() {
            assert!(!admitted("stop apps-lf-x.slice ssh.service y.slice"));
        }

        // TC: SETUP-02
        #[test]
        fn refuses_stop_when_the_unit_is_not_an_environment_slice() {
            assert!(!admitted("stop ssh.service"));
        }

        // TC: SETUP-02
        #[test]
        fn refuses_set_property_when_it_outlives_a_reboot() {
            assert!(!admitted(
                "set-property apps-lf-x.slice CPUQuota=100% MemoryMax=1024M MemorySwapMax=0"
            ));
        }

        // TC: SETUP-02
        #[test]
        fn refuses_set_property_when_it_lifts_the_swap_cap() {
            assert!(!admitted(
                "set-property --runtime apps-lf-x.slice CPUQuota=100% MemoryMax=1024M MemorySwapMax=infinity"
            ));
        }
    }
}
