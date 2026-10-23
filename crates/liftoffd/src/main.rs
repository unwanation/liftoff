//! `liftoffd` binary: everything lives in the library so tests can reach it.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    liftoffd::main_entry().await
}
