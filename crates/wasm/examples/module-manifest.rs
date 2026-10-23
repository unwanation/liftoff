//! Prints the manifest of a built module component, checked as the
//! daemon checks it.
//!
//! ```text
//! cargo run -q --locked -p liftoff-wasm --example module-manifest -- <component.wasm>
//! ```
//!
//! `modules/registry/release.sh` reads a release's name, version,
//! `requires` and `config_schema` from what this prints, so the index of
//! the registry repeats the component's own manifest and nothing written
//! by hand next to it. The check is the daemon's own: a [`ModuleHost`]
//! with the daemon's default `[modules]` limits (memory cap, call
//! timeout) and its host imports reads the manifest through
//! [`ModuleHost::inspect`], as an install does, so a component this
//! accepts is one `liftoffd` accepts and a `manifest()` that never
//! returns times out here too. Its storage keeps nothing and its log
//! goes to `tracing`, which nothing subscribes to: stdout carries the
//! manifest only.

use core::time::Duration;
use std::io::Write;
use std::process::ExitCode;

use liftoff_core::domain::ModuleName;
use liftoff_core::error::Error;
use liftoff_core::ports::ModuleStore;
use liftoff_wasm::{Limits, ModuleHost, TokioTimer, TracingLog};

/// The daemon's default `[modules]` limits (`max_memory_mb = 64`,
/// `call_timeout = "10s"`).
const LIMITS: Limits = Limits {
    max_memory_bytes: 64 * 1024 * 1024,
    call_timeout: Duration::from_secs(10),
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        say(
            &mut std::io::stderr(),
            "usage: module-manifest <component.wasm>",
        );
        return ExitCode::from(64);
    };
    match manifest(std::path::Path::new(&path)).await {
        Ok(json) => {
            say(&mut std::io::stdout(), &json);
            ExitCode::SUCCESS
        }
        Err(error) => {
            say(
                &mut std::io::stderr(),
                &format!("module-manifest: {}: {error}", path.display()),
            );
            ExitCode::FAILURE
        }
    }
}

/// Writes `line` to `out`; a closed stream has nobody left to tell.
fn say(out: &mut impl Write, line: &str) {
    let _ignored: std::io::Result<()> = writeln!(out, "{line}");
}

/// The storage of the check: it keeps nothing, so nothing the component
/// does while it is inspected outlives the check.
struct Discard;

impl ModuleStore for Discard {
    fn module_get(
        &self,
        _module: &ModuleName,
        _key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        core::future::ready(Ok(None))
    }

    fn module_set(
        &self,
        _module: &ModuleName,
        _key: &str,
        _value: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        core::future::ready(Ok(()))
    }

    fn module_query(
        &self,
        _module: &ModuleName,
        _sql: &str,
        _params: &[String],
    ) -> impl Future<Output = Result<String, Error>> + Send {
        core::future::ready(Ok("[]".to_owned()))
    }

    fn module_purge(&self, _module: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        core::future::ready(Ok(()))
    }
}

/// The manifest of the component at `path` as JSON (`name`, `version`,
/// `requires`, `config_schema`), refused unless [`ModuleHost::inspect`]
/// accepts it.
async fn manifest(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let host = ModuleHost::new(Discard, TokioTimer, TracingLog, LIMITS)?;
    // The name the host files the check under (its errors say it): the
    // file's stem with `_` read as `.` (`dns_cloudflare.wasm` is
    // `dns.cloudflare`), else `component`. The manifest names the module
    // itself.
    let label = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| ModuleName::new(&stem.replace('_', ".")).ok())
        .map_or_else(|| ModuleName::new("component"), Ok)?;
    let manifest = host.inspect(label, &bytes).await?;
    let json = serde_json::json!({
        "name": manifest.name.to_string(),
        "version": manifest.version,
        "requires": manifest
            .requires
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "config_schema": manifest.config_schema,
    });
    Ok(json.to_string())
}
