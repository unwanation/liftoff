use axum::extract::State;
use axum::{Extension, Json};
use liftoff_api::{ServerCheckView, ServerView};
use liftoff_core::ports::{Ports, Proxy as _};
use liftoff_core::service::WildcardVerdict;
use serde::Serialize;

use crate::error::ApiError;
use crate::ready::{Probed, ReadyCheck};
use crate::services::Services;

/// Generic status body for probes.
#[derive(Debug, Serialize)]
pub(crate) struct Status {
    status: &'static str,
}

pub(crate) async fn healthz() -> Json<Status> {
    Json(Status { status: "ok" })
}

/// Every dependency: the installed [`Readiness`](crate::Readiness) check
/// (docker and `SQLite`, none without one), then the proxy's admin API
/// (`GET /config/` on Caddy). Each is asked whatever the others answered.
async fn dependencies<P: Ports>(
    services: &Services<P>,
    check: Option<Extension<ReadyCheck>>,
) -> Vec<Probed> {
    let mut probed = match check {
        Some(Extension(ReadyCheck(Some(check)))) => check.check().await,
        _ => Vec::new(),
    };
    probed.push(Probed {
        name: "caddy",
        result: services.ports.proxy().ready().await,
    });
    probed
}

/// Logs the cause of every failed dependency; `true` when none failed.
fn all_ready(probed: &[Probed]) -> bool {
    let mut ready = true;
    for Probed { name, result } in probed {
        if let Err(error) = result {
            tracing::error!(dependency = name, %error, "not ready");
            ready = false;
        }
    }
    ready
}

/// Readiness of the whole API: `503` without the cause, which
/// goes to the log only, when any dependency fails.
pub(crate) async fn readyz_with_proxy<P: Ports>(
    State(services): State<Services<P>>,
    check: Option<Extension<ReadyCheck>>,
) -> Result<Json<Status>, ApiError> {
    if all_ready(&dependencies(&services, check).await) {
        Ok(Json(Status { status: "ready" }))
    } else {
        Err(ApiError::unavailable())
    }
}

/// `GET /server` (behind a session): the base domain, the public IP and
/// one row per dependency, then the wildcard DNS of the base, for `lof
/// doctor`. Always `200`: a failed dependency is
/// a row, its cause goes to the log only.
pub(crate) async fn server<P: Ports>(
    State(services): State<Services<P>>,
    check: Option<Extension<ReadyCheck>>,
) -> Json<ServerView> {
    let probed = dependencies(&services, check).await;
    let _ready = all_ready(&probed);
    let mut checks: Vec<ServerCheckView> = probed
        .into_iter()
        .map(|Probed { name, result }| row(name, if result.is_ok() { "ok" } else { "fail" }, None))
        .collect();
    let settings = &services.settings;
    let (base_domain, public_ip) = (settings.base.to_string(), settings.public_ip.to_string());
    checks.push(match services.domains.wildcard_dns().await {
        Ok(report) => match report.verdict {
            WildcardVerdict::PointsAtUs => row(
                "dns",
                "ok",
                Some(format!("*.{} → {}", report.base, report.public_ip)),
            ),
            WildcardVerdict::Elsewhere(addresses) => row(
                "dns",
                "warn",
                Some(format!(
                    "{} resolves to {}, not {} (a proxied or stale wildcard?)",
                    report.probe,
                    join(&addresses),
                    report.public_ip
                )),
            ),
            WildcardVerdict::Missing => row(
                "dns",
                "fail",
                Some(format!(
                    "{} does not resolve: add a wildcard record *.{} → {}",
                    report.probe, report.base, report.public_ip
                )),
            ),
        },
        Err(error) => {
            tracing::error!(%error, "wildcard DNS check failed");
            row(
                "dns",
                "fail",
                Some("the resolver did not answer".to_owned()),
            )
        }
    });
    Json(ServerView {
        base_domain,
        public_ip,
        checks,
    })
}

fn row(name: &str, status: &str, detail: Option<String>) -> ServerCheckView {
    ServerCheckView {
        name: name.to_owned(),
        status: status.to_owned(),
        detail,
    }
}

fn join(addresses: &[core::net::Ipv4Addr]) -> String {
    addresses
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) async fn not_found() -> ApiError {
    ApiError::not_found()
}
