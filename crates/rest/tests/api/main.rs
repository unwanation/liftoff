//! Black-box tests driving the router in memory, one module per route
//! group (`REST-01…10`).

#![cfg(test)]

mod admin;
mod artifacts;
mod artifacts_view;
mod auth;
mod bindings;
mod cors;
mod create_plan;
mod deploys;
mod domains;
mod environments;
mod function_deploys;
mod git;
mod internal;
mod links;
mod login;
mod modules;
mod openapi;
mod probes;
mod projects;
mod readyz;
mod registry;
mod secrets;
mod support;
mod templates;
mod tunnel;
mod visibility;
