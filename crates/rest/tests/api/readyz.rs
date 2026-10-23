//! `/readyz` asks the installed `Readiness` check.

use core::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use liftoff_core::Error;
use liftoff_core::fakes::World;
use liftoff_rest::{AppState, Probed, Readiness};
use tower::ServiceExt as _;

use crate::support::app;

/// A check whose verdict the test flips.
#[derive(Debug, Default)]
struct Switch {
    down: AtomicBool,
}

#[derive(Debug, Clone, Default)]
struct SharedSwitch(Arc<Switch>);

impl Readiness for SharedSwitch {
    fn check(&self) -> Pin<Box<dyn Future<Output = Vec<Probed>> + Send + '_>> {
        Box::pin(async {
            let docker = if self.0.down.load(Ordering::SeqCst) {
                Err(Error::unavailable("docker is down"))
            } else {
                Ok(())
            };
            vec![
                Probed {
                    name: "docker",
                    result: docker,
                },
                Probed {
                    name: "sqlite",
                    result: Ok(()),
                },
            ]
        })
    }
}

async fn get(state: AppState, path: &str) -> http::Response<Body> {
    app(&World::new().await, state)
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn state_with(check: Option<SharedSwitch>) -> AppState {
    let state = AppState::new();
    match check {
        Some(check) => state.with_readiness(check),
        None => state,
    }
}

#[tokio::test]
async fn ok_when_every_check_passes() {
    let response = get(state_with(Some(SharedSwitch::default())), "/readyz").await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unavailable_without_the_cause_when_a_check_fails() {
    let switch = SharedSwitch::default();
    switch.0.down.store(true, Ordering::SeqCst);
    let response = get(state_with(Some(switch)), "/api/v1/readyz").await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    assert_eq!(
        (status, body.contains("docker")),
        (StatusCode::SERVICE_UNAVAILABLE, false),
        "503 and no cause: {body}"
    );
}

#[tokio::test]
async fn ok_when_no_check_is_installed() {
    let response = get(state_with(None), "/readyz").await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// `GET /api/v1/server` as the owner over `world` with `state`'s check.
async fn server(world: &World, state: AppState) -> (StatusCode, serde_json::Value) {
    let secret = world.login(&world.owner).await.unwrap();
    let response = app(world, state)
        .oneshot(
            Request::get("/api/v1/server")
                .header("authorization", format!("Bearer {}", secret.expose()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// `(name, status)` of each row of a `ServerView`.
fn rows(view: &serde_json::Value) -> Vec<(String, String)> {
    view["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap().to_owned(),
                row["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn expect(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(name, status)| ((*name).to_owned(), (*status).to_owned()))
        .collect()
}

mod server {
    use std::net::Ipv4Addr;

    use liftoff_core::domain::Host;

    use super::*;

    fn apps() -> Host {
        Host::new("apps.test").unwrap()
    }

    // TC: CLIX-09 (docker, SQLite and Caddy admin apart, the wildcard DNS of the base)
    #[tokio::test]
    async fn lists_each_dependency_and_the_base_when_all_answer() {
        let world = World::new().await;
        world
            .ports
            .resolver
            .set_wildcard_a(&apps(), vec![Ipv4Addr::new(203, 0, 113, 10)]);
        let (status, view) = server(&world, state_with(Some(SharedSwitch::default()))).await;
        assert_eq!(
            (
                status,
                view["base_domain"].as_str(),
                view["public_ip"].as_str(),
                rows(&view)
            ),
            (
                StatusCode::OK,
                Some("apps.test"),
                Some("203.0.113.10"),
                expect(&[
                    ("docker", "ok"),
                    ("sqlite", "ok"),
                    ("caddy", "ok"),
                    ("dns", "ok")
                ])
            ),
            "{view}"
        );
    }

    // TC: CLIX-09 (a failed dependency is a row, its cause stays in the log)
    #[tokio::test]
    async fn marks_only_docker_failed_without_the_cause_when_docker_is_down() {
        let world = World::new().await;
        world
            .ports
            .resolver
            .set_wildcard_a(&apps(), vec![Ipv4Addr::new(203, 0, 113, 10)]);
        let switch = SharedSwitch::default();
        switch.0.down.store(true, Ordering::SeqCst);
        let (status, view) = server(&world, state_with(Some(switch))).await;
        assert_eq!(
            (
                status,
                rows(&view),
                view.to_string().contains("docker is down")
            ),
            (
                StatusCode::OK,
                expect(&[
                    ("docker", "fail"),
                    ("sqlite", "ok"),
                    ("caddy", "ok"),
                    ("dns", "ok")
                ]),
                false
            ),
            "{view}"
        );
    }

    // TC: CLIX-09
    #[tokio::test]
    async fn marks_caddy_failed_when_the_admin_api_is_down() {
        let world = World::new().await;
        world
            .ports
            .resolver
            .set_wildcard_a(&apps(), vec![Ipv4Addr::new(203, 0, 113, 10)]);
        world.ports.proxy.fail_always(
            "ready",
            Error::upstream(liftoff_core::System::Caddy, "connection refused"),
        );
        let (_, view) = server(&world, state_with(None)).await;
        assert_eq!(
            rows(&view),
            expect(&[("caddy", "fail"), ("dns", "ok")]),
            "{view}"
        );
    }

    // TC: CLIX-09 (a wildcard elsewhere warns, none fails)
    #[tokio::test]
    async fn warns_when_the_wildcard_resolves_elsewhere_and_fails_without_one() {
        let elsewhere = World::new().await;
        elsewhere
            .ports
            .resolver
            .set_wildcard_a(&apps(), vec![Ipv4Addr::new(198, 51, 100, 7)]);
        let (_, warned) = server(&elsewhere, state_with(None)).await;
        let (_, missing) = server(&World::new().await, state_with(None)).await;
        assert_eq!(
            (
                rows(&warned),
                warned["checks"][1]["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("198.51.100.7")),
                rows(&missing)
            ),
            (
                expect(&[("caddy", "ok"), ("dns", "warn")]),
                true,
                expect(&[("caddy", "ok"), ("dns", "fail")])
            ),
            "{warned} {missing}"
        );
    }
}
