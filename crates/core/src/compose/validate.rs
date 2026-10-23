//! The compose rules, collected into a list of [`ComposeError`]s.
//!
//! Beyond the letter of the spec, three tightenings close ways to reach
//! another project's resources: a top-level `volumes`/`networks` entry may
//! not set `name` (it would attach an existing volume or network of another
//! target), a network `driver` may only be `bridge`, and anonymous volumes
//! (`- /data`) are refused because only named volumes are allowed.

use std::collections::BTreeSet;

use super::yaml::{Mapping, Value};

use super::model::{ComposeError, ComposeFile};
use crate::domain::ServiceName;

const TOP_LEVEL: &[&str] = &["services", "volumes", "networks", "configs", "name"];

const SERVICE_ALLOWED: &[&str] = &[
    "image",
    "command",
    "entrypoint",
    "user",
    "working_dir",
    "labels",
    "expose",
    "stop_grace_period",
    "configs",
    "read_only",
    "cap_drop",
    "healthcheck",
    "depends_on",
    "environment",
    "env_file",
    "volumes",
    "tmpfs",
    "networks",
    "logging",
    "restart",
];

const SERVICE_FORBIDDEN: &[&str] = &[
    "build",
    "privileged",
    "cap_add",
    "devices",
    "security_opt",
    "network_mode",
    "pid",
    "ipc",
    "userns_mode",
    "cgroup_parent",
    "ports",
    "extends",
    "sysctls",
    "ulimits",
    "platform",
    "volumes_from",
    "runtime",
    "deploy",
];

const VOLUME_FORBIDDEN: &[&str] = &["external", "driver", "driver_opts", "name"];
const VOLUME_ALLOWED: &[&str] = &["labels"];
const NETWORK_FORBIDDEN: &[&str] = &["external", "driver_opts", "ipam", "name"];
const NETWORK_ALLOWED: &[&str] = &["labels", "internal", "attachable", "enable_ipv6", "driver"];

const FORBIDDEN: &str = "forbidden";
const UNKNOWN: &str = "unknown key";

/// Names declared at the top level.
struct Declared {
    volumes: BTreeSet<String>,
    networks: BTreeSet<String>,
    configs: BTreeSet<String>,
}

impl Declared {
    fn of(root: &Mapping) -> Self {
        let names = |section: &str| -> BTreeSet<String> {
            root.get(section)
                .and_then(Value::as_mapping)
                .into_iter()
                .flat_map(Mapping::keys)
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect()
        };
        Self {
            volumes: names("volumes"),
            networks: names("networks"),
            configs: names("configs"),
        }
    }
}

pub(super) fn validate(file: &ComposeFile) -> Result<(), Vec<ComposeError>> {
    let declared = Declared::of(&file.root);
    let mut errors = Vec::new();
    for (key, value) in &file.root {
        let Some(key) = key.as_str() else {
            errors.push(ComposeError::new("", "top-level keys must be strings"));
            continue;
        };
        match key {
            "services" => services(value, &declared, &mut errors),
            "volumes" => plain_section(
                value,
                "volumes",
                VOLUME_FORBIDDEN,
                VOLUME_ALLOWED,
                &mut errors,
            ),
            "networks" => networks_section(value, &mut errors),
            "configs" => configs_section(value, &mut errors),
            "include" => errors.push(ComposeError::new(key, FORBIDDEN)),
            "secrets" => secrets(value, &mut errors),
            _ if TOP_LEVEL.contains(&key) => {}
            _ => errors.push(ComposeError::new(key, UNKNOWN)),
        }
    }
    if !file.root.contains_key("services") {
        errors.push(ComposeError::new("services", "required"));
    }
    errors.sort();
    errors.dedup();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn entries(value: &Value) -> impl Iterator<Item = (&str, &Value)> {
    value
        .as_mapping()
        .into_iter()
        .flat_map(Mapping::iter)
        .filter_map(|(k, v)| Some((k.as_str()?, v)))
}

fn services(value: &Value, declared: &Declared, errors: &mut Vec<ComposeError>) {
    if !value.is_mapping() {
        errors.push(ComposeError::new("services", "must be a mapping"));
        return;
    }
    for (name, body) in entries(value) {
        let path = format!("services.{name}");
        if ServiceName::new(name).is_err() {
            errors.push(ComposeError::new(&path, "invalid service name"));
        }
        service(body, &path, declared, errors);
    }
}

fn service(body: &Value, path: &str, declared: &Declared, errors: &mut Vec<ComposeError>) {
    let empty = Mapping::new();
    let body = match body {
        Value::Null => &empty,
        Value::Mapping(map) => map,
        _ => {
            errors.push(ComposeError::new(path, "must be a mapping"));
            return;
        }
    };
    match body.get("image") {
        None | Some(Value::Null) => {
            errors.push(ComposeError::new(format!("{path}.image"), "required"));
        }
        Some(Value::String(image)) if !image.trim().is_empty() => {}
        Some(_) => errors.push(ComposeError::new(
            format!("{path}.image"),
            "must be a non-empty string",
        )),
    }
    for (key, value) in body {
        let Some(key) = key.as_str() else {
            errors.push(ComposeError::new(path, "keys must be strings"));
            continue;
        };
        let at = format!("{path}.{key}");
        if SERVICE_FORBIDDEN.contains(&key) {
            errors.push(ComposeError::new(at, FORBIDDEN));
        } else if !SERVICE_ALLOWED.contains(&key) {
            errors.push(ComposeError::new(at, UNKNOWN));
        } else {
            service_key(key, value, &at, declared, errors);
        }
    }
}

fn service_key(
    key: &str,
    value: &Value,
    at: &str,
    declared: &Declared,
    errors: &mut Vec<ComposeError>,
) {
    match key {
        "volumes" => service_volumes(value, at, declared, errors),
        "networks" => service_networks(value, at, declared, errors),
        "configs" => service_configs(value, at, declared, errors),
        "env_file" => env_files(value, at, errors),
        _ => {}
    }
}

fn items(value: &Value) -> impl Iterator<Item = (usize, &Value)> {
    value.as_sequence().into_iter().flatten().enumerate()
}

fn service_volumes(value: &Value, at: &str, declared: &Declared, errors: &mut Vec<ComposeError>) {
    if !value.is_sequence() {
        errors.push(ComposeError::new(at, "must be a list"));
        return;
    }
    for (i, item) in items(value) {
        let here = format!("{at}[{i}]");
        match item {
            Value::String(short) => short_volume(short, &here, declared, errors),
            Value::Mapping(long) => long_volume(long, &here, declared, errors),
            _ => errors.push(ComposeError::new(here, "must be a string or a mapping")),
        }
    }
}

const BIND_FORBIDDEN: &str = "bind mounts are forbidden";
const NO_VARIABLES: &str = "variables are not allowed in names and volume sources";

fn undeclared_volume(name: &str) -> String {
    format!("volume \"{name}\" is not declared in top-level volumes")
}

fn short_volume(short: &str, here: &str, declared: &Declared, errors: &mut Vec<ComposeError>) {
    let Some((source, rest)) = short.split_once(':') else {
        errors.push(ComposeError::new(
            here,
            "anonymous volumes are not allowed, declare a named volume",
        ));
        return;
    };
    let drive_letter = source.len() == 1
        && source.chars().all(|c| c.is_ascii_alphabetic())
        && rest.starts_with(['/', '\\']);
    if source.contains('$') {
        errors.push(ComposeError::new(here, NO_VARIABLES));
    } else if drive_letter || source.is_empty() || source.starts_with(['/', '.', '~']) {
        errors.push(ComposeError::new(here, BIND_FORBIDDEN));
    } else if !declared.volumes.contains(source) {
        errors.push(ComposeError::new(here, undeclared_volume(source)));
    }
}

fn long_volume(long: &Mapping, here: &str, declared: &Declared, errors: &mut Vec<ComposeError>) {
    match long.get("type").and_then(Value::as_str) {
        Some("tmpfs") => {}
        Some("volume") => match long.get("source").and_then(Value::as_str) {
            Some(source) if source.contains('$') => {
                errors.push(ComposeError::new(format!("{here}.source"), NO_VARIABLES));
            }
            Some(source) if declared.volumes.contains(source) => {}
            Some(source) => errors.push(ComposeError::new(
                format!("{here}.source"),
                undeclared_volume(source),
            )),
            None => errors.push(ComposeError::new(format!("{here}.source"), "required")),
        },
        Some("bind") => errors.push(ComposeError::new(format!("{here}.type"), BIND_FORBIDDEN)),
        Some(other) => errors.push(ComposeError::new(
            format!("{here}.type"),
            format!("unsupported volume type \"{other}\""),
        )),
        None => errors.push(ComposeError::new(format!("{here}.type"), "required")),
    }
}

fn undeclared_network(name: &str) -> String {
    format!("network \"{name}\" is not declared in top-level networks")
}

fn service_networks(value: &Value, at: &str, declared: &Declared, errors: &mut Vec<ComposeError>) {
    let known = |name: &str| name == "default" || declared.networks.contains(name);
    match value {
        Value::Sequence(_) => {
            for (i, item) in items(value) {
                match item.as_str() {
                    Some(name) if name.contains('$') => {
                        errors.push(ComposeError::new(format!("{at}[{i}]"), NO_VARIABLES));
                    }
                    Some(name) if known(name) => {}
                    Some(name) => errors.push(ComposeError::new(
                        format!("{at}[{i}]"),
                        undeclared_network(name),
                    )),
                    None => {
                        errors.push(ComposeError::new(format!("{at}[{i}]"), "must be a string"));
                    }
                }
            }
        }
        Value::Mapping(_) => {
            for (name, _) in entries(value) {
                if name.contains('$') {
                    errors.push(ComposeError::new(format!("{at}.{name}"), NO_VARIABLES));
                } else if !known(name) {
                    errors.push(ComposeError::new(
                        format!("{at}.{name}"),
                        undeclared_network(name),
                    ));
                }
            }
        }
        Value::Null => {}
        _ => errors.push(ComposeError::new(at, "must be a list or a mapping")),
    }
}

fn service_configs(value: &Value, at: &str, declared: &Declared, errors: &mut Vec<ComposeError>) {
    for (i, item) in items(value) {
        let name = match item {
            Value::String(name) => Some(name.as_str()),
            Value::Mapping(map) => map.get("source").and_then(Value::as_str),
            _ => None,
        };
        match name {
            Some(name) if name.contains('$') => {
                errors.push(ComposeError::new(format!("{at}[{i}]"), NO_VARIABLES));
            }
            Some(name) if declared.configs.contains(name) => {}
            Some(name) => errors.push(ComposeError::new(
                format!("{at}[{i}]"),
                format!("config \"{name}\" is not declared in top-level configs"),
            )),
            None => errors.push(ComposeError::new(
                format!("{at}[{i}]"),
                "must be a config name or a mapping with `source`",
            )),
        }
    }
}

const ENV_FILE: &str = ".env";

fn env_files(value: &Value, at: &str, errors: &mut Vec<ComposeError>) {
    let bad = |path: String| ComposeError::new(path, format!("must be \"{ENV_FILE}\""));
    match value {
        Value::String(file) if file != ENV_FILE => errors.push(bad(at.to_owned())),
        Value::Sequence(_) => {
            for (i, item) in items(value) {
                let file = match item {
                    Value::String(file) => Some(file.as_str()),
                    Value::Mapping(map) => map.get("path").and_then(Value::as_str),
                    _ => None,
                };
                if file != Some(ENV_FILE) {
                    errors.push(bad(format!("{at}[{i}]")));
                }
            }
        }
        _ => {}
    }
}

/// `volumes` / `networks`: entries may be empty or carry only `allowed`
/// keys; `forbidden` keys are reported as such, anything else as unknown.
fn plain_section(
    value: &Value,
    section: &str,
    forbidden: &[&str],
    allowed: &[&str],
    errors: &mut Vec<ComposeError>,
) {
    for (name, body) in entries(value) {
        if name.contains('$') {
            errors.push(ComposeError::new(format!("{section}.{name}"), NO_VARIABLES));
        }
        for (key, _) in entries(body) {
            let at = format!("{section}.{name}.{key}");
            if forbidden.contains(&key) {
                errors.push(ComposeError::new(at, FORBIDDEN));
            } else if !allowed.contains(&key) {
                errors.push(ComposeError::new(at, UNKNOWN));
            }
        }
    }
}

fn networks_section(value: &Value, errors: &mut Vec<ComposeError>) {
    plain_section(
        value,
        "networks",
        NETWORK_FORBIDDEN,
        NETWORK_ALLOWED,
        errors,
    );
    for (name, body) in entries(value) {
        let driver = body.get("driver").and_then(Value::as_str);
        if driver.is_some_and(|d| d != "bridge") {
            errors.push(ComposeError::new(
                format!("networks.{name}.driver"),
                FORBIDDEN,
            ));
        }
    }
}

fn configs_section(value: &Value, errors: &mut Vec<ComposeError>) {
    for (name, body) in entries(value) {
        if name.contains('$') {
            errors.push(ComposeError::new(format!("configs.{name}"), NO_VARIABLES));
        }
        if body.get("content").is_none() {
            errors.push(ComposeError::new(
                format!("configs.{name}.content"),
                "required",
            ));
        }
        for (key, _) in entries(body).filter(|(key, _)| *key != "content") {
            errors.push(ComposeError::new(
                format!("configs.{name}.{key}"),
                FORBIDDEN,
            ));
        }
    }
}

fn secrets(value: &Value, errors: &mut Vec<ComposeError>) {
    let mut with_file = false;
    for (name, body) in entries(value) {
        if body.get("file").is_some() {
            with_file = true;
            errors.push(ComposeError::new(format!("secrets.{name}.file"), FORBIDDEN));
        }
    }
    if !with_file {
        errors.push(ComposeError::new("secrets", UNKNOWN));
    }
}
