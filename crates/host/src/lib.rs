//! Adapters of the server host: the local processes `docker compose` and
//! `sudo systemctl`, the compose project directories, the kept components
//! of function deploys ([`FileComponentFiles`]), and the host's view
//! of public DNS ([`DnsResolver`], moved here from `liftoff-remote` when that crate went away in). Each implements a port
//! from `liftoff-core`. Caddy is driven through its admin API by
//! `liftoff-proxy`.
//!
//! Command construction is separated from execution: every adapter builds a
//! `Cmd` (program, arguments, environment) with a pure function that the
//! tests compare argument by argument, and only then hands it to the runner.

mod cmd;
mod components;
mod dns;
mod docker;
mod files;
mod systemd;

pub use components::FileComponentFiles;
pub use dns::DnsResolver;
pub use docker::ProcessDocker;
pub use files::FileProjectFiles;
pub use systemd::SudoSystemd;
