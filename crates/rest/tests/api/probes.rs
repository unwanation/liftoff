//! The middleware every answer of the application router goes through:
//! problem+json for unknown routes, hardening headers, request IDs, and
//! `/healthz`.

use http::{StatusCode, header};

use crate::support::WorldCtx;

#[tokio::test]
async fn problem_json_404_when_route_unknown() {
    let ctx = WorldCtx::new().await;
    let reply = ctx.call("GET", "/nope", None, None).await;
    assert_eq!(
        (
            reply.status,
            reply.headers[header::CONTENT_TYPE].to_str().unwrap()
        ),
        (StatusCode::NOT_FOUND, "application/problem+json")
    );
}

#[tokio::test]
async fn ok_when_healthz() {
    let ctx = WorldCtx::new().await;
    let reply = ctx.call("GET", "/healthz", None, None).await;
    assert_eq!(
        (reply.status, reply.json()),
        (StatusCode::OK, serde_json::json!({"status": "ok"}))
    );
}

#[tokio::test]
async fn hardening_headers_when_any_response() {
    let ctx = WorldCtx::new().await;
    let reply = ctx.call("GET", "/healthz", None, None).await;
    let headers = &reply.headers;
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        "default-src 'none'; frame-ancestors 'none'"
    );
}

#[tokio::test]
async fn generates_request_id_when_absent() {
    let ctx = WorldCtx::new().await;
    let reply = ctx.call("GET", "/healthz", None, None).await;
    assert!(reply.headers.contains_key("x-request-id"));
}

#[tokio::test]
async fn echoes_id_when_client_sends_request_id() {
    let ctx = WorldCtx::new().await;
    let reply = ctx
        .call_with("GET", "/healthz", &[("x-request-id", "abc-123")], None)
        .await;
    assert_eq!(reply.headers["x-request-id"], "abc-123");
}
