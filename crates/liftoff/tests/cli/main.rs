//! Black-box tests of the CLI against a recording local stub of the API,
//! one module per case of the phase-1 catalogue (`CLIX-01…09`), plus what
//! the v1 skeleton kept (`v1 CLI-01`, `CLI-02`, `CLI-04`), and local mode
//! (`local`, `LCL-01…05`, `LCL-08`), and `create`,
//! `templates` and `tenant use` with a server
//! (`CRE-07`, `CRE-09`, `CRE-11…17`, `TPL-03`, `TPL-04`, `TPL-06`), and
//! `lof expose`, `lof env share|unshare` and the tunnel in `lof status`
//! (`expose`, `EXP-09…12`, `DEVT-09`, `DEVT-10`), and `lof bind <host>` of a
//! function environment (`bind`), and `lof artifact`
//! (`artifacts`, `CLIA-01…05`).

#![cfg(test)]

mod artifacts;
mod batch;
mod bind;
mod create;
mod doctor;
mod errors;
mod expose;
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
mod run;
mod secrets;
mod servers;
mod session;
mod snippet;
mod support;
mod templates;
mod tenant_use;
mod tenants;
