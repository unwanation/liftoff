//! Black-box tests of the CLI against a recording local stub of the API,
//! one module per case of the phase-1 catalogue (`CLIX-01…09`), plus what
//! the v1 skeleton kept (`v1 CLI-01`, `CLI-02`, `CLI-04`), and local mode
//! (`local`, `LCL-01…05`, `LCL-08`), and `create`,
//! `templates` and `tenant use` with a server
//! (`CRE-07`, `CRE-09`, `CRE-11…17`, `TPL-03`, `TPL-04`, `TPL-06`), and
//! `lof dev` of (`DEV-01…05`), and `lof dev` with the tunnel,
//! `lof env share|unshare` and the tunnel in `lof status`
//! (`dev_tunnel`, `DEVT-09…12`), and `lof bind <host>` of a
//! function environment (`bind`), and `lof artifact`
//! (`artifacts`, `CLIA-01…05`).

#![cfg(test)]

mod artifacts;
mod batch;
mod bind;
mod create;
mod dev;
mod dev_tunnel;
mod doctor;
mod errors;
mod fixtures;
mod function_deploy;
mod git;
mod install;
mod interactive;
mod json;
mod local;
mod login;
mod mcp;
mod modules;
mod not_logged_in;
mod output;
mod project;
mod secrets;
mod servers;
mod session;
mod snippet;
mod support;
mod templates;
mod tenant_use;
mod tenants;
