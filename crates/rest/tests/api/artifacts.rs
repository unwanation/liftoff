//! `/api/v1/artifacts`: publish
//! as `multipart/form-data` (`html`, a `files` field per file named by
//! its `filename`, `title`) and as JSON with base64 files (the MCP form),
//! update by `PUT`, list, get, versions, rollback, delete, delete a
//! version, share and unshare, with `401`, `403` and `404`
//! (`ART-15…18`).
//!
//! The world's API host is `liftoff.test`, its clock stands at
//! 2026-10-13T00:00:00Z, its `[artifacts]` are TTL 5 hours, 16 MiB, 50
//! versions; its owner (an admin) publishes every artifact; `carol` is a
//! stored user who is no admin, and so may not publish without modules
//! (amendment of).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{StatusCode, header};
use liftoff_core::domain::{ArtifactId, ArtifactPath, LinkSecret, VersionNumber};
use liftoff_core::fakes::World;
use liftoff_core::ports::Store as _;
use liftoff_core::service::{ArtifactContent, ArtifactUpload};
use rstest::rstest;
use serde_json::{Value, json};

use crate::support::{Reply, WorldCtx, detail, is_problem, strings};

/// The collection.
const ARTIFACTS: &str = "/api/v1/artifacts";

/// The prefix of every one-time link of the world.
const LINK_PREFIX: &str = "https://liftoff.test/api/v1/auth/links/";

/// The cookie of the `browser` session on the API host.
const API_COOKIE: &str = "__Host-liftoff_session";

/// The HTML of version 1: 15 bytes.
const HTML: &str = "<h1>Report</h1>";

/// A stylesheet of 13 bytes.
const CSS: &[u8] = b"h1{color:red}";

/// The 8-byte PNG signature, a file that is no UTF-8.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The boundary of every multipart body here.
const BOUNDARY: &str = "liftoff-test-boundary";

/// A well-formed artifact id nobody holds.
const UNKNOWN: &str = "aB3dE6gH9jK2";

/// One field of a multipart body.
struct Part<'a> {
    name: &'a str,
    filename: Option<&'a str>,
    bytes: &'a [u8],
}

/// The text field `name`.
const fn field<'a>(name: &'a str, value: &'a str) -> Part<'a> {
    Part {
        name,
        filename: None,
        bytes: value.as_bytes(),
    }
}

/// A `files` field: the file `filename` with `bytes`.
const fn file<'a>(filename: &'a str, bytes: &'a [u8]) -> Part<'a> {
    Part {
        name: "files",
        filename: Some(filename),
        bytes,
    }
}

/// The `multipart/form-data` body of `parts`.
fn multipart(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part.filename {
            Some(filename) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{filename}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n",
                    part.name
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                    part.name
                )
                .as_bytes(),
            ),
        }
        body.extend_from_slice(part.bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// `Bearer <fresh cli session of the owner>`.
async fn owner_bearer(ctx: &WorldCtx) -> String {
    let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
    format!("Bearer {}", secret.expose())
}

/// Sends `method path` as the owner with the multipart body of `parts`.
async fn owner_multipart(ctx: &WorldCtx, method: &str, path: &str, parts: &[Part<'_>]) -> Reply {
    let bearer = owner_bearer(ctx).await;
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
    ctx.raw(
        None,
        method,
        path,
        &[
            (header::AUTHORIZATION.as_str(), bearer.as_str()),
            (header::CONTENT_TYPE.as_str(), content_type.as_str()),
        ],
        multipart(parts),
    )
    .await
}

/// Sends `method path` as the owner with the raw JSON text `body`.
async fn owner_json_bytes(ctx: &WorldCtx, method: &str, path: &str, body: Vec<u8>) -> Reply {
    let bearer = owner_bearer(ctx).await;
    ctx.raw(
        None,
        method,
        path,
        &[
            (header::AUTHORIZATION.as_str(), bearer.as_str()),
            (header::CONTENT_TYPE.as_str(), "application/json"),
        ],
        body,
    )
    .await
}

/// The multipart publish of version 1: [`HTML`] titled `Report` with
/// `style.css` and `img/logo.png`.
async fn publish_multipart(ctx: &WorldCtx) -> Reply {
    owner_multipart(
        ctx,
        "POST",
        ARTIFACTS,
        &[
            field("html", HTML),
            field("title", "Report"),
            file("style.css", CSS),
            file("img/logo.png", PNG),
        ],
    )
    .await
}

/// The JSON publish of [`HTML`] with `img/logo.png` in base64.
async fn publish_json(ctx: &WorldCtx) -> Reply {
    let body = json!({
        "html": HTML,
        "title": "Report",
        "files": {"img/logo.png": STANDARD.encode(PNG)},
    });
    ctx.owner("POST", ARTIFACTS, Some(&body)).await
}

/// The id of the artifact `reply` answers with.
fn id_of(reply: &Reply) -> ArtifactId {
    ArtifactId::parse(reply.json()["id"].as_str().unwrap_or_default())
        .unwrap_or_else(|error| panic!("{error}: {}", reply.text()))
}

/// Version `n`.
fn version(n: u32) -> VersionNumber {
    VersionNumber::new(n).unwrap()
}

/// The names of the files of version `n` of `id` as stored.
async fn stored_names(world: &World, id: ArtifactId, n: u32) -> Vec<String> {
    world
        .ports
        .store
        .artifact_version(id, version(n))
        .await
        .unwrap()
        .map(|stored| {
            stored
                .files
                .iter()
                .map(|file| file.name.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The bytes of `name` of version `n` of `id`; empty when there is none.
async fn content(world: &World, id: ArtifactId, n: u32, name: &str) -> Vec<u8> {
    let name = if name == ArtifactPath::INDEX {
        ArtifactPath::index()
    } else {
        ArtifactPath::new(name).unwrap()
    };
    world
        .artifacts()
        .content(id, version(n), &name)
        .await
        .unwrap_or_default()
}

/// The content of a version titled `title` with `files`.
fn version_content(html: &str, title: &str, files: &[(&str, &[u8])]) -> ArtifactContent {
    ArtifactContent {
        html: html.as_bytes().to_vec(),
        files: files
            .iter()
            .map(|(name, bytes)| ArtifactUpload {
                name: ArtifactPath::new(name).unwrap(),
                bytes: bytes.to_vec(),
            })
            .collect(),
        title: Some(title.to_owned()),
    }
}

/// Arranges version 1 of a private artifact of the owner through the
/// service: [`HTML`] titled `Report` with `style.css`, 28 bytes.
async fn arranged(world: &World) -> ArtifactId {
    world
        .artifacts()
        .publish(
            &world.owner,
            version_content(HTML, "Report", &[("style.css", CSS)]),
        )
        .await
        .unwrap()
        .artifact
        .id
}

/// Adds version `title` (`<h1>Second</h1>`, 15 bytes, no files) to `id`
/// an hour later than the clock stood.
async fn arranged_update(world: &World, id: ArtifactId, title: &str) {
    world.ports.clock.advance(time::Duration::hours(1));
    let _published = world
        .artifacts()
        .update(
            &world.owner,
            id,
            version_content("<h1>Second</h1>", title, &[]),
        )
        .await
        .unwrap();
}

/// `/api/v1/artifacts/<id><rest>`.
fn at(id: impl core::fmt::Display, rest: &str) -> String {
    format!("{ARTIFACTS}/{id}{rest}")
}

/// The world with `[artifacts] max_size_mb` of 2 MiB.
async fn small_world() -> WorldCtx {
    WorldCtx::with_settings(|settings| settings.artifacts.max_size_bytes = 2 * 1024 * 1024).await
}

mod publish {
    use super::*;

    // TC: ART-15 (version 1 of a private artifact whose TTL runs from
    // now: 2026-10-13T00:00:00Z + 5 hours)
    #[tokio::test]
    async fn created_private_with_ttl_when_owner_posts_multipart() {
        let ctx = WorldCtx::new().await;
        let reply = publish_multipart(&ctx).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["shared"].clone(),
                body["expires_at"].clone(),
                body["version"].clone(),
                body["title"].clone(),
            ),
            (
                StatusCode::CREATED,
                json!(false),
                json!("2026-10-13T05:00:00Z"),
                json!(1),
                json!("Report"),
            ),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (`https://<base>/a/<id>`)
    #[tokio::test]
    async fn url_on_api_host_names_id_when_owner_posts_multipart() {
        let ctx = WorldCtx::new().await;
        let reply = publish_multipart(&ctx).await;
        let id = id_of(&reply);
        assert_eq!(
            reply.json()["url"].as_str(),
            Some(format!("https://liftoff.test/a/{id}").as_str()),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (a private artifact's publish carries a one-time link to log a browser in)
    #[tokio::test]
    async fn login_url_is_one_time_link_when_published_private() {
        let ctx = WorldCtx::new().await;
        let reply = publish_multipart(&ctx).await;
        let login_url = reply.json()["login_url"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let token = login_url.strip_prefix(LINK_PREFIX).unwrap_or_default();
        assert_eq!(
            (reply.status, LinkSecret::parse(token).is_ok()),
            (StatusCode::CREATED, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (the `login_url` opens a `browser` session on the API
    // host and lands on the artifact)
    #[tokio::test]
    async fn login_url_lands_on_artifact_with_session_when_followed() {
        let ctx = WorldCtx::new().await;
        let reply = publish_multipart(&ctx).await;
        let body = reply.json();
        let path = body["login_url"]
            .as_str()
            .and_then(|url| url.strip_prefix("https://liftoff.test"))
            .unwrap_or("/api/v1/auth/links/missing")
            .to_owned();
        let opened = ctx
            .call_with(
                "GET",
                &path,
                &[(header::HOST.as_str(), "liftoff.test")],
                None,
            )
            .await;
        let cookie = opened
            .headers
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            (
                opened.status,
                opened
                    .headers
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok()),
                cookie.starts_with(&format!("{API_COOKIE}=")),
            ),
            (StatusCode::FOUND, body["url"].as_str(), true),
            "{}",
            opened.text()
        );
    }

    // TC: ART-15 (with `[artifacts] host` the API host's cookie cannot reach the artifact: the link lands on the gate's handover to that host, amendment of)
    #[tokio::test]
    async fn login_url_lands_on_gate_handover_when_artifacts_host_set() {
        let ctx = WorldCtx::with_settings(|settings| {
            settings.artifacts.host =
                Some(liftoff_core::domain::Host::new("artifacts.test").unwrap());
        })
        .await;
        let reply = publish_multipart(&ctx).await;
        let body = reply.json();
        let id = id_of(&reply);
        let path = body["login_url"]
            .as_str()
            .and_then(|url| url.strip_prefix("https://liftoff.test"))
            .unwrap_or("/api/v1/auth/links/missing")
            .to_owned();
        let opened = ctx
            .call_with(
                "GET",
                &path,
                &[(header::HOST.as_str(), "liftoff.test")],
                None,
            )
            .await;
        assert_eq!(
            (
                opened.status,
                opened
                    .headers
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            ),
            (
                StatusCode::FOUND,
                Some(format!(
                    "https://liftoff.test/gate/start?host=artifacts.test&rd=%2F{id}"
                )),
            ),
            "{}",
            opened.text()
        );
    }

    // TC: ART-15 (each `files` field is stored under its `filename`;
    // the HTML is `index.html`, then the files by path)
    #[tokio::test]
    async fn stores_files_under_their_filenames_when_multipart() {
        let ctx = WorldCtx::new().await;
        let reply = publish_multipart(&ctx).await;
        let id = id_of(&reply);
        assert_eq!(
            stored_names(&ctx.world, id, 1).await,
            vec!["index.html", "img/logo.png", "style.css"]
        );
    }

    // TC: ART-15 (a binary file arrives byte for byte)
    #[tokio::test]
    async fn keeps_file_bytes_when_multipart() {
        let ctx = WorldCtx::new().await;
        let reply = publish_multipart(&ctx).await;
        let id = id_of(&reply);
        assert_eq!(content(&ctx.world, id, 1, "img/logo.png").await, PNG);
    }

    // TC: ART-15 (the field `html` is the version's `index.html`)
    #[tokio::test]
    async fn stores_html_as_index_when_multipart() {
        let ctx = WorldCtx::new().await;
        let reply = publish_multipart(&ctx).await;
        let id = id_of(&reply);
        assert_eq!(
            content(&ctx.world, id, 1, "index.html").await,
            HTML.as_bytes()
        );
    }

    // TC: ART-15 (the MCP form: `{html, title, files: {name: base64}}`)
    #[tokio::test]
    async fn created_when_json_form_has_base64_files() {
        let ctx = WorldCtx::new().await;
        let reply = publish_json(&ctx).await;
        assert_eq!(
            (reply.status, reply.json()["version"].clone()),
            (StatusCode::CREATED, json!(1)),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (base64 is decoded before the file is stored)
    #[tokio::test]
    async fn stores_decoded_bytes_when_json_form() {
        let ctx = WorldCtx::new().await;
        let reply = publish_json(&ctx).await;
        let id = id_of(&reply);
        assert_eq!(content(&ctx.world, id, 1, "img/logo.png").await, PNG);
    }

    // TC: ART-15 (the JSON form answers as the multipart one: a login
    // link for the private artifact)
    #[tokio::test]
    async fn login_url_present_when_json_form() {
        let ctx = WorldCtx::new().await;
        let reply = publish_json(&ctx).await;
        assert!(
            reply.json()["login_url"]
                .as_str()
                .is_some_and(|url| url.starts_with(LINK_PREFIX)),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (ART-02 at the boundary: the error names the file)
    #[tokio::test]
    async fn bad_request_naming_file_when_filename_has_dotdot() {
        let ctx = WorldCtx::new().await;
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), file("../x.css", CSS)],
        )
        .await;
        assert_eq!(
            (reply.status, detail(&reply).starts_with("files.../x.css: ")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (ART-02 in the JSON form)
    #[tokio::test]
    async fn bad_request_naming_file_when_json_name_is_index() {
        let ctx = WorldCtx::new().await;
        let body = json!({"html": HTML, "files": {"index.html": STANDARD.encode(CSS)}});
        let reply = ctx.owner("POST", ARTIFACTS, Some(&body)).await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("files.index.html: ")
            ),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (content that is no base64 names its file)
    #[tokio::test]
    async fn bad_request_naming_file_when_content_not_base64() {
        let ctx = WorldCtx::new().await;
        let body = json!({"html": HTML, "files": {"img/logo.png": "not base64!"}});
        let reply = ctx.owner("POST", ARTIFACTS, Some(&body)).await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("files.img/logo.png: ")
            ),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (`deny_unknown_fields`)
    #[tokio::test]
    async fn bad_request_when_json_field_unknown() {
        let ctx = WorldCtx::new().await;
        let body = json!({"html": HTML, "file": {}});
        let reply = ctx.owner("POST", ARTIFACTS, Some(&body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    }

    // TC: ART-15 (a multipart body without `html` names the field)
    #[tokio::test]
    async fn bad_request_naming_html_when_multipart_has_none() {
        let ctx = WorldCtx::new().await;
        let reply = owner_multipart(&ctx, "POST", ARTIFACTS, &[file("style.css", CSS)]).await;
        assert_eq!(
            (reply.status, detail(&reply).starts_with("html: ")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15, CLIA-01 (the form `lof artifact publish` sends: the
    // page as the field `html` with `filename="index.html"`)
    #[tokio::test]
    async fn created_when_html_field_carries_filename() {
        let ctx = WorldCtx::new().await;
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[
                Part {
                    name: "html",
                    filename: Some("index.html"),
                    bytes: HTML.as_bytes(),
                },
                file("style.css", CSS),
            ],
        )
        .await;
        let id = id_of(&reply);
        assert_eq!(
            (reply.status, content(&ctx.world, id, 1, "index.html").await),
            (StatusCode::CREATED, HTML.as_bytes().to_vec()),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (a `files` field names its file by its `filename`)
    #[tokio::test]
    async fn bad_request_naming_files_when_files_field_has_no_filename() {
        let ctx = WorldCtx::new().await;
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), field("files", "h1{color:red}")],
        )
        .await;
        assert_eq!(
            (reply.status, detail(&reply).starts_with("files: ")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (the multipart form rejects unknown fields too)
    #[tokio::test]
    async fn bad_request_naming_field_when_multipart_field_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), field("colour", "red")],
        )
        .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("colour")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (a version larger than the 1 MiB of any other body but
    // within `max_size_mb`, 16 MiB here, is accepted)
    #[tokio::test]
    async fn created_when_version_above_default_body_limit() {
        let ctx = WorldCtx::new().await;
        let big = vec![b'x'; 2 * 1024 * 1024];
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), file("data.bin", &big)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    }

    // TC: ART-15 (the JSON form gets the room of its base64: 2 MiB of
    // bytes are 2.7 MiB of JSON)
    #[tokio::test]
    async fn created_when_json_version_above_default_body_limit() {
        let ctx = WorldCtx::new().await;
        let big = vec![b'x'; 2 * 1024 * 1024];
        let body = json!({"html": HTML, "files": {"data.bin": STANDARD.encode(&big)}});
        let reply = ctx.owner("POST", ARTIFACTS, Some(&body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    }

    // TC: ART-15 (ART-03 at the boundary: a version above `max_size_mb`
    // whose body is still within the route's limit is the service's
    // `400` on `files`)
    #[tokio::test]
    async fn bad_request_naming_files_when_version_over_max_size() {
        let ctx = small_world().await;
        let big = vec![b'x'; 2 * 1024 * 1024];
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), file("data.bin", &big)],
        )
        .await;
        assert_eq!(
            (reply.status, detail(&reply).starts_with("files: ")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (the multipart body gets 256 KiB of headroom over
    // `max_size_mb`, 2 MiB here: 200 KiB over it is still read and is the
    // service's `400` on `files`)
    #[tokio::test]
    async fn bad_request_naming_files_when_multipart_within_headroom() {
        let ctx = small_world().await;
        let big = vec![b'x'; 2 * 1024 * 1024 + 200 * 1024];
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), file("data.bin", &big)],
        )
        .await;
        assert_eq!(
            (reply.status, detail(&reply).starts_with("files: ")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (300 KiB over `max_size_mb` is past the headroom: `413`)
    #[tokio::test]
    async fn payload_too_large_when_multipart_past_headroom() {
        let ctx = small_world().await;
        let big = vec![b'x'; 2 * 1024 * 1024 + 300 * 1024];
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), file("data.bin", &big)],
        )
        .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::PAYLOAD_TOO_LARGE, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (the JSON body gets `ceil(max / 3) * 4` plus the same
    // headroom: 150 KiB over `max_size_mb` in base64 is still read and is
    // the service's `400` on `files`)
    #[tokio::test]
    async fn bad_request_naming_files_when_json_within_headroom() {
        let ctx = small_world().await;
        let big = vec![b'x'; 2 * 1024 * 1024 + 150 * 1024];
        let body = json!({"html": HTML, "files": {"data.bin": STANDARD.encode(&big)}});
        let reply =
            owner_json_bytes(&ctx, "POST", ARTIFACTS, serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(
            (reply.status, detail(&reply).starts_with("files: ")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (200 KiB over `max_size_mb` in base64 is past it: `413`)
    #[tokio::test]
    async fn payload_too_large_when_json_past_headroom() {
        let ctx = small_world().await;
        let big = vec![b'x'; 2 * 1024 * 1024 + 200 * 1024];
        let body = json!({"html": HTML, "files": {"data.bin": STANDARD.encode(&big)}});
        let reply =
            owner_json_bytes(&ctx, "POST", ARTIFACTS, serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::PAYLOAD_TOO_LARGE, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (the body limit is `max_size_mb` plus headroom: four
    // times the limit is refused with `413` before it is read)
    #[tokio::test]
    async fn payload_too_large_when_multipart_far_over_max_size() {
        let ctx = small_world().await;
        let big = vec![b'x'; 8 * 1024 * 1024];
        let reply = owner_multipart(
            &ctx,
            "POST",
            ARTIFACTS,
            &[field("html", HTML), file("data.bin", &big)],
        )
        .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::PAYLOAD_TOO_LARGE, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-15 (the same for the JSON form)
    #[tokio::test]
    async fn payload_too_large_when_json_far_over_max_size() {
        let ctx = small_world().await;
        let big = vec![b'x'; 8 * 1024 * 1024];
        let body = json!({"html": HTML, "files": {"data.bin": STANDARD.encode(&big)}});
        let reply =
            owner_json_bytes(&ctx, "POST", ARTIFACTS, serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::PAYLOAD_TOO_LARGE, true),
            "{}",
            reply.text()
        );
    }
}

mod update {
    use super::*;

    // TC: ART-16 (`PUT` as multipart: version 2 is the current one)
    #[tokio::test]
    async fn new_current_version_when_owner_puts_multipart() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = owner_multipart(
            &ctx,
            "PUT",
            &at(id, ""),
            &[field("html", "<h1>Second</h1>"), field("title", "Second")],
        )
        .await;
        let body = reply.json();
        assert_eq!(
            (reply.status, body["version"].clone(), body["title"].clone()),
            (StatusCode::OK, json!(2), json!("Second")),
            "{}",
            reply.text()
        );
    }

    // TC: ART-16 (the one-time login link is publish's alone)
    #[tokio::test]
    async fn leaves_out_login_url_when_updated() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = owner_multipart(
            &ctx,
            "PUT",
            &at(id, ""),
            &[field("html", "<h1>Second</h1>")],
        )
        .await;
        assert_eq!(
            (reply.status, reply.json().get("login_url").cloned()),
            (StatusCode::OK, None),
            "{}",
            reply.text()
        );
    }

    // TC: ART-16 (the address stays)
    #[tokio::test]
    async fn same_url_when_updated() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = owner_multipart(
            &ctx,
            "PUT",
            &at(id, ""),
            &[field("html", "<h1>Second</h1>")],
        )
        .await;
        assert_eq!(
            reply.json()["url"].as_str(),
            Some(format!("https://liftoff.test/a/{id}").as_str()),
            "{}",
            reply.text()
        );
    }

    // TC: ART-16 (ART-07 at the REST seam: the TTL of a private artifact
    // runs from its latest version, an hour later here)
    #[tokio::test]
    async fn ttl_from_now_when_private_artifact_updated() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        ctx.world.ports.clock.advance(time::Duration::hours(1));
        let reply = owner_multipart(
            &ctx,
            "PUT",
            &at(id, ""),
            &[field("html", "<h1>Second</h1>")],
        )
        .await;
        assert_eq!(
            (reply.status, reply.json()["expires_at"].clone()),
            (StatusCode::OK, json!("2026-10-13T06:00:00Z")),
            "{}",
            reply.text()
        );
    }

    // TC: ART-16 (`PUT` as JSON: the new version has its own files only,
    // HTML and files as a whole)
    #[tokio::test]
    async fn files_of_new_version_only_when_owner_puts_json() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let body = json!({
            "html": "<h1>Second</h1>",
            "files": {"notes.txt": STANDARD.encode(b"two")},
        });
        let reply = ctx.owner("PUT", &at(id, ""), Some(&body)).await;
        assert_eq!(
            (reply.status, stored_names(&ctx.world, id, 2).await),
            (
                StatusCode::OK,
                vec!["index.html".to_owned(), "notes.txt".to_owned()]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: ART-16 (the bytes of the JSON form are decoded)
    #[tokio::test]
    async fn stores_decoded_bytes_when_owner_puts_json() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let body = json!({
            "html": "<h1>Second</h1>",
            "files": {"notes.txt": STANDARD.encode(b"two")},
        });
        let _reply = ctx.owner("PUT", &at(id, ""), Some(&body)).await;
        assert_eq!(content(&ctx.world, id, 2, "notes.txt").await, b"two");
    }

    // TC: ART-16 (version 1 stays as it was: `/v/1`)
    #[tokio::test]
    async fn keeps_previous_version_when_updated() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let body = json!({"html": "<h1>Second</h1>"});
        let reply = ctx.owner("PUT", &at(id, ""), Some(&body)).await;
        assert_eq!(
            (reply.status, content(&ctx.world, id, 1, "index.html").await),
            (StatusCode::OK, HTML.as_bytes().to_vec()),
            "{}",
            reply.text()
        );
    }
}

mod versions {
    use super::*;

    // TC: ART-17 (`n`, `title`, `size` with the HTML, `created_at`;
    // oldest first)
    #[tokio::test]
    async fn lists_number_title_size_and_date_when_two_versions() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        arranged_update(&ctx.world, id, "Second").await;
        let reply = ctx.owner("GET", &at(id, "/versions"), None).await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::OK,
                json!([
                    {"n": 1, "title": "Report", "size": 28, "created_at": "2026-10-13T00:00:00Z"},
                    {"n": 2, "title": "Second", "size": 15, "created_at": "2026-10-13T01:00:00Z"},
                ])
            ),
            "{}",
            reply.text()
        );
    }
}

mod list {
    use super::*;

    // TC: ART-17 (`GET /artifacts`: the caller's, newest first)
    #[tokio::test]
    async fn lists_owners_artifacts_newest_first() {
        let ctx = WorldCtx::new().await;
        let first = arranged(&ctx.world).await;
        ctx.world.ports.clock.advance(time::Duration::minutes(1));
        let second = arranged(&ctx.world).await;
        let reply = ctx.owner("GET", ARTIFACTS, None).await;
        assert_eq!(
            (reply.status, strings(&reply.json(), "id")),
            (StatusCode::OK, vec![second.to_string(), first.to_string()]),
            "{}",
            reply.text()
        );
    }

    // TC: ART-18 (ART-12 at the REST seam: `?all=true` is the admin's)
    #[tokio::test]
    async fn forbidden_when_carol_asks_for_all() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol("GET", &format!("{ARTIFACTS}?all=true"), None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }
}

mod get {
    use super::*;

    // TC: ART-17 (every answer about an artifact carries `id`, `url`,
    // `shared`, `expires_at`, `version`)
    #[tokio::test]
    async fn answers_artifact_when_owner_asks() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = ctx.owner("GET", &at(id, ""), None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["id"].clone(),
                body["url"].clone(),
                body["title"].clone(),
                body["shared"].clone(),
                body["expires_at"].clone(),
                body["version"].clone(),
            ),
            (
                StatusCode::OK,
                json!(id.to_string()),
                json!(format!("https://liftoff.test/a/{id}")),
                json!("Report"),
                json!(false),
                json!("2026-10-13T05:00:00Z"),
                json!(1),
            ),
            "{}",
            reply.text()
        );
    }

    // TC: ART-17 (the one-time login link is publish's alone)
    #[tokio::test]
    async fn leaves_out_login_url_when_owner_asks() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = ctx.owner("GET", &at(id, ""), None).await;
        assert_eq!(
            (reply.status, reply.json().get("login_url").cloned()),
            (StatusCode::OK, None),
            "{}",
            reply.text()
        );
    }
}

mod rollback {
    use super::*;

    // TC: ART-17 (ART-09 at the REST seam: `{to: 1}` makes version 3 with
    // the title of version 1)
    #[tokio::test]
    async fn copies_version_as_new_current_when_owner_rolls_back() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        arranged_update(&ctx.world, id, "Second").await;
        let reply = ctx
            .owner("POST", &at(id, "/rollback"), Some(&json!({"to": 1})))
            .await;
        let body = reply.json();
        assert_eq!(
            (reply.status, body["version"].clone(), body["title"].clone()),
            (StatusCode::OK, json!(3), json!("Report")),
            "{}",
            reply.text()
        );
    }

    // TC: ART-18 (an unknown version is `404`)
    #[tokio::test]
    async fn not_found_when_version_unknown() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = ctx
            .owner("POST", &at(id, "/rollback"), Some(&json!({"to": 7})))
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    }

    // TC: ART-17 (`deny_unknown_fields`)
    #[tokio::test]
    async fn bad_request_when_body_field_unknown() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = ctx
            .owner(
                "POST",
                &at(id, "/rollback"),
                Some(&json!({"to": 1, "from": 2})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    }
}

mod delete_version {
    use super::*;

    // TC: ART-17 (ART-10 at the REST seam: the current version stays)
    #[tokio::test]
    async fn conflict_when_version_current() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        arranged_update(&ctx.world, id, "Second").await;
        let reply = ctx.owner("DELETE", &at(id, "/versions/2"), None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-17 (ART-10: another version goes)
    #[tokio::test]
    async fn drops_version_when_not_current() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        arranged_update(&ctx.world, id, "Second").await;
        let reply = ctx.owner("DELETE", &at(id, "/versions/1"), None).await;
        let left = ctx.owner("GET", &at(id, "/versions"), None).await;
        let numbers: Vec<Value> = left
            .json()
            .as_array()
            .map(|items| items.iter().map(|item| item["n"].clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            (reply.status, numbers),
            (StatusCode::OK, vec![json!(2)]),
            "{}",
            reply.text()
        );
    }
}

mod delete {
    use super::*;

    // TC: ART-17 (ART-14 at the REST seam: the answer names the id)
    #[tokio::test]
    async fn answers_deleted_id_when_owner_deletes() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = ctx.owner("DELETE", &at(id, ""), None).await;
        assert_eq!(
            (reply.status, reply.json()),
            (StatusCode::OK, json!({"deleted": id.to_string()})),
            "{}",
            reply.text()
        );
    }

    // TC: ART-18 (a deleted artifact is unknown)
    #[tokio::test]
    async fn not_found_when_asked_after_delete() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let _deleted = ctx.owner("DELETE", &at(id, ""), None).await;
        let reply = ctx.owner("GET", &at(id, ""), None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    }
}

mod visibility {
    use super::*;

    // TC: ART-17 (ART-06 at the REST seam: shared for good)
    #[tokio::test]
    async fn shared_without_expiry_when_owner_shares() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = ctx.owner("POST", &at(id, "/share"), None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["shared"].clone(),
                body["expires_at"].clone()
            ),
            (StatusCode::OK, json!(true), Value::Null),
            "{}",
            reply.text()
        );
    }

    // TC: ART-17 (ART-06: unshare starts a new TTL from now, two hours
    // later here)
    #[tokio::test]
    async fn private_with_ttl_from_now_when_owner_unshares() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let _shared = ctx.world.artifacts().share(&ctx.world.owner, id).await;
        ctx.world.ports.clock.advance(time::Duration::hours(2));
        let reply = ctx.owner("POST", &at(id, "/unshare"), None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["shared"].clone(),
                body["expires_at"].clone()
            ),
            (StatusCode::OK, json!(false), json!("2026-10-13T07:00:00Z")),
            "{}",
            reply.text()
        );
    }
}

mod refusals {
    use super::*;

    // TC: ART-18 (no session: `401` problem on every route of artifacts)
    #[rstest]
    #[case::list("GET", "")]
    #[case::publish("POST", "")]
    #[case::get("GET", "/aB3dE6gH9jK2")]
    #[case::update("PUT", "/aB3dE6gH9jK2")]
    #[case::delete("DELETE", "/aB3dE6gH9jK2")]
    #[case::versions("GET", "/aB3dE6gH9jK2/versions")]
    #[case::rollback("POST", "/aB3dE6gH9jK2/rollback")]
    #[case::delete_version("DELETE", "/aB3dE6gH9jK2/versions/1")]
    #[case::share("POST", "/aB3dE6gH9jK2/share")]
    #[case::unshare("POST", "/aB3dE6gH9jK2/unshare")]
    #[tokio::test]
    async fn unauthorized_when_no_session(#[case] method: &str, #[case] rest: &str) {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call(method, &format!("{ARTIFACTS}{rest}"), None, None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-18 (ART-11 at the REST seam: another user's artifact is
    // `403` for every route that names it)
    #[rstest]
    #[case::get("GET", "", None)]
    #[case::update("PUT", "", Some(json!({"html": "<p>mine now</p>"})))]
    #[case::delete("DELETE", "", None)]
    #[case::versions("GET", "/versions", None)]
    #[case::rollback("POST", "/rollback", Some(json!({"to": 1})))]
    #[case::delete_version("DELETE", "/versions/1", None)]
    #[case::share("POST", "/share", None)]
    #[case::unshare("POST", "/unshare", None)]
    #[tokio::test]
    async fn forbidden_when_artifact_is_another_users(
        #[case] method: &str,
        #[case] rest: &str,
        #[case] body: Option<Value>,
    ) {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        arranged_update(&ctx.world, id, "Second").await;
        let reply = ctx.carol(method, &at(id, rest), body.as_ref()).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-18 (an unknown artifact is `404` for every route that
    // names it)
    #[rstest]
    #[case::get("GET", "", None)]
    #[case::update("PUT", "", Some(json!({"html": "<p>new</p>"})))]
    #[case::delete("DELETE", "", None)]
    #[case::versions("GET", "/versions", None)]
    #[case::rollback("POST", "/rollback", Some(json!({"to": 1})))]
    #[case::delete_version("DELETE", "/versions/1", None)]
    #[case::share("POST", "/share", None)]
    #[case::unshare("POST", "/unshare", None)]
    #[tokio::test]
    async fn not_found_when_artifact_unknown(
        #[case] method: &str,
        #[case] rest: &str,
        #[case] body: Option<Value>,
    ) {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner(method, &at(UNKNOWN, rest), body.as_ref()).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: ART-18 (an unknown version of a known artifact is `404`)
    #[tokio::test]
    async fn not_found_when_deleted_version_unknown() {
        let ctx = WorldCtx::new().await;
        let id = arranged(&ctx.world).await;
        let reply = ctx.owner("DELETE", &at(id, "/versions/7"), None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    }

    // TC: ART-18 (without modules only the admin publishes)
    #[tokio::test]
    async fn forbidden_when_carol_publishes_without_modules() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol("POST", ARTIFACTS, Some(&json!({"html": HTML})))
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }
}

mod document {
    use super::*;

    // TC: ART-15, ART-16, ART-17 (the routes of artifacts are in `/openapi.json`)
    #[tokio::test]
    async fn artifact_routes_documented_when_generated() {
        let ctx = WorldCtx::new().await;
        let doc = ctx.call("GET", "/openapi.json", None, None).await.json();
        let missing: Vec<String> = [
            ("get", "/api/v1/artifacts"),
            ("post", "/api/v1/artifacts"),
            ("get", "/api/v1/artifacts/{id}"),
            ("put", "/api/v1/artifacts/{id}"),
            ("delete", "/api/v1/artifacts/{id}"),
            ("get", "/api/v1/artifacts/{id}/versions"),
            ("post", "/api/v1/artifacts/{id}/rollback"),
            ("delete", "/api/v1/artifacts/{id}/versions/{n}"),
            ("post", "/api/v1/artifacts/{id}/share"),
            ("post", "/api/v1/artifacts/{id}/unshare"),
        ]
        .into_iter()
        .filter(|(method, path)| !doc["paths"][*path][*method].is_object())
        .map(|(method, path)| format!("{method} {path}"))
        .collect();
        assert_eq!(missing, Vec::<String>::new());
    }

    // TC: ART-15, ART-17 (the wire types of artifacts carry `ToSchema`)
    #[tokio::test]
    async fn artifact_schemas_listed_when_generated() {
        let ctx = WorldCtx::new().await;
        let doc = ctx.call("GET", "/openapi.json", None, None).await.json();
        let missing: Vec<&str> = [
            "ArtifactBody",
            "ArtifactView",
            "ArtifactVersionView",
            "ArtifactRollback",
        ]
        .into_iter()
        .filter(|name| !doc["components"]["schemas"][*name].is_object())
        .collect();
        assert_eq!(missing, Vec::<&str>::new());
    }
}
