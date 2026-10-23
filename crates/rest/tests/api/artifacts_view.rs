//! The viewer of artifacts:
//! the wrapper page `GET /a/{id}[/v/{n}]` with its top bar and version
//! switcher, the files of a version `GET /a/{id}/v/{n}/{file}` in an
//! opaque origin, access through `Access::check` with the target
//! `artifact:<id>`, and the same routes on `[artifacts] host`
//! (`VIEW-01…08`), plus the owner's share and unshare buttons of the
//! wrapper (`ART-06`).
//!
//! The world's API host is `liftoff.test`; its owner (an admin) publishes
//! every artifact; `carol` is another user.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use http::{StatusCode, header};
use liftoff_core::domain::{
    ArtifactId, ArtifactPath, Host, Login, User, UserRole, VersionNumber, Visibility,
};
use liftoff_core::fakes::World;
use liftoff_core::ports::Store as _;
use liftoff_core::service::{ArtifactContent, ArtifactUpload};

use crate::support::{Reply, WorldCtx};

/// The world's API host.
const API: &str = "liftoff.test";

/// The artifacts' own host of the VIEW-08 installation.
const ARTIFACTS: &str = "artifacts.test";

/// The cookie of the `browser` session on the API host.
const API_COOKIE: &str = "__Host-liftoff_session";

/// The CSP of every file of a version (an opaque origin; the wrapper of its own host alone may frame it, as the standalone artifacts server).
const SANDBOX_CSP: &str =
    "sandbox allow-scripts allow-forms allow-popups allow-modals; frame-ancestors 'self'";

/// The `sandbox` attribute of the wrapper's iframe.
const IFRAME_SANDBOX: &str = r#"sandbox="allow-scripts allow-forms allow-popups allow-modals""#;

/// The HTML of every version: a script whose marker must never reach the
/// wrapper page.
const PAGE: &str = "<!doctype html>\n<title>Report</title>\n<p>hello from the artifact</p>\n\
                    <script>window.marker = 'artifact-script'</script>\n";

/// The content of a version titled `title` with `files`.
fn content(title: &str, files: &[(&str, &[u8])]) -> ArtifactContent {
    ArtifactContent {
        html: PAGE.as_bytes().to_vec(),
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

/// Publishes version 1 of a private artifact of the owner.
async fn publish(world: &World, title: &str, files: &[(&str, &[u8])]) -> ArtifactId {
    world
        .artifacts()
        .publish(&world.owner, content(title, files))
        .await
        .unwrap()
        .artifact
        .id
}

/// Publishes a shared artifact of the owner (version 1).
async fn shared(world: &World, title: &str, files: &[(&str, &[u8])]) -> ArtifactId {
    let id = publish(world, title, files).await;
    let _shared = world.artifacts().share(&world.owner, id).await.unwrap();
    id
}

/// Adds a version titled `title` to `id`.
async fn update(world: &World, id: ArtifactId, title: &str) {
    let _published = world
        .artifacts()
        .update(&world.owner, id, content(title, &[]))
        .await
        .unwrap();
}

/// The stored user `carol`, who is no admin and owns nothing.
async fn carol(world: &World) -> User {
    let login = Login::new("carol").unwrap();
    match world.ports.store.user_by_login(&login).await.unwrap() {
        Some(carol) => carol,
        None => world.add_user("carol", UserRole::User).await.unwrap(),
    }
}

/// The `Cookie` line of a fresh `browser` session of `user` on the API
/// host.
async fn cookie_of(world: &World, user: &User) -> String {
    let secret = world.browser_login(user).await.unwrap();
    format!("{API_COOKIE}={}", secret.expose())
}

/// `GET path` on `host` as a browser, with the `Cookie` line `cookie`.
async fn get(ctx: &WorldCtx, host: &str, path: &str, cookie: Option<&str>) -> Reply {
    let mut headers = vec![
        (header::HOST.as_str(), host),
        (header::ACCEPT.as_str(), "text/html,application/xhtml+xml"),
    ];
    if let Some(cookie) = cookie {
        headers.push((header::COOKIE.as_str(), cookie));
    }
    ctx.call_with("GET", path, &headers, None).await
}

/// A form's `POST path` on `host` from the page of `origin` (none: no
/// `Origin` header), with the `Cookie` line `cookie`.
async fn post_form(
    ctx: &WorldCtx,
    host: &str,
    path: &str,
    origin: Option<&str>,
    cookie: Option<&str>,
) -> Reply {
    let mut headers = vec![
        (header::HOST.as_str(), host),
        (
            header::CONTENT_TYPE.as_str(),
            "application/x-www-form-urlencoded",
        ),
    ];
    if let Some(origin) = origin {
        headers.push((header::ORIGIN.as_str(), origin));
    }
    if let Some(cookie) = cookie {
        headers.push((header::COOKIE.as_str(), cookie));
    }
    ctx.raw(None, "POST", path, &headers, Vec::new()).await
}

/// The `Cookie` line of `user`'s session on the artifacts' host, got as
/// a browser gets it: `/gate/start` on the API host with the API host's
/// `browser` session, then the callback on the artifacts' host with the
/// code (the v1 SSO handover).
async fn artifacts_cookie_of(ctx: &WorldCtx, user: &User, id: ArtifactId) -> String {
    let api_cookie = cookie_of(&ctx.world, user).await;
    let start = get(
        ctx,
        API,
        &format!("/gate/start?host={ARTIFACTS}&rd=%2F{id}"),
        Some(&api_cookie),
    )
    .await;
    let callback = location(&start);
    let path = callback
        .strip_prefix(&format!("https://{ARTIFACTS}"))
        .unwrap_or_default()
        .to_owned();
    let entered = get(ctx, ARTIFACTS, &path, None).await;
    header_values(&entered, &header::SET_COOKIE)
        .into_iter()
        .next()
        .and_then(|line| line.split(';').next().map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// The values of `name` in `reply`.
fn header_values(reply: &Reply, name: &header::HeaderName) -> Vec<String> {
    reply
        .headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok().map(ToOwned::to_owned))
        .collect()
}

/// The `Location` of `reply`; empty without one.
fn location(reply: &Reply) -> String {
    header_values(reply, &header::LOCATION)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The `Content-Type` of `reply`; empty without one.
fn content_type(reply: &Reply) -> String {
    header_values(reply, &header::CONTENT_TYPE)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The opening `<iframe …>` tag of `body`, if any.
fn iframe(body: &str) -> Option<String> {
    let rest = body.get(body.find("<iframe")?..)?;
    rest.get(..=rest.find('>')?).map(ToOwned::to_owned)
}

/// The opening tag of `rest`, which starts with `<`.
fn opening_tag(rest: &str) -> Option<&str> {
    rest.get(..=rest.find('>')?)
}

/// Whether `body` has a `<form>` posting to `action`.
fn has_form_posting_to(body: &str, action: &str) -> bool {
    body.match_indices("<form").any(|(start, _)| {
        body.get(start..).and_then(opening_tag).is_some_and(|tag| {
            tag.contains(&format!(r#"action="{action}""#))
                && tag.to_ascii_lowercase().contains(r#"method="post""#)
        })
    })
}

/// The visibility of `id` as stored.
async fn visibility_of(world: &World, id: ArtifactId) -> Visibility {
    world
        .ports
        .store
        .artifact(id)
        .await
        .unwrap()
        .unwrap()
        .visibility
}

mod wrapper {
    use super::*;

    // TC: VIEW-01
    #[tokio::test]
    async fn serves_wrapper_when_shared_without_cookie() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), None).await;
        assert_eq!(
            (reply.status, content_type(&reply)),
            (StatusCode::OK, "text/html; charset=utf-8".to_owned())
        );
    }

    // TC: VIEW-01 (the iframe is sandboxed and shows the current version)
    #[tokio::test]
    async fn frames_current_version_in_sandbox_when_shared_without_cookie() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "First draft", &[]).await;
        update(&ctx.world, id, "Second draft").await;
        let reply = get(&ctx, API, &format!("/a/{id}"), None).await;
        let frame = iframe(&reply.text()).unwrap_or_default();
        assert!(
            frame.contains(IFRAME_SANDBOX)
                && frame.contains(&format!(r#"src="/a/{id}/v/2/index.html""#)),
            "{frame}"
        );
    }

    // TC: VIEW-01 (the HTML never runs in the API's origin)
    #[tokio::test]
    async fn never_inlines_artifact_html_when_shared() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), None).await;
        let body = reply.text();
        assert!(
            reply.status == StatusCode::OK
                && !body.contains("artifact-script")
                && !body.contains("hello from the artifact"),
            "{body}"
        );
    }

    // TC: VIEW-01 (the top bar: the title, escaped)
    #[tokio::test]
    async fn shows_escaped_title_when_shared() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Q3 <b>numbers</b>", &[]).await;
        let body = get(&ctx, API, &format!("/a/{id}"), None).await.text();
        assert!(
            body.contains("Q3 &lt;b&gt;numbers&lt;/b&gt;") && !body.contains("<b>numbers"),
            "{body}"
        );
    }

    // TC: VIEW-01 (the top bar: a link to the raw HTML of the version)
    #[tokio::test]
    async fn links_raw_html_of_version_when_shared() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let body = get(&ctx, API, &format!("/a/{id}"), None).await.text();
        assert!(
            body.contains(&format!(r#"href="/a/{id}/v/1/index.html""#)),
            "{body}"
        );
    }

    // TC: VIEW-01 (the daemon's page headers; no script at all, frames of its own origin only)
    #[tokio::test]
    async fn carries_page_headers_without_scripts_when_shared() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), None).await;
        let csp = header_values(&reply, &header::CONTENT_SECURITY_POLICY);
        assert_eq!(
            (
                csp.len(),
                csp.iter().all(|csp| csp.starts_with("default-src 'none';")
                    && csp.contains("frame-src 'self'")
                    && csp.contains("frame-ancestors 'none'")
                    && !csp.contains("script")),
                header_values(&reply, &header::X_CONTENT_TYPE_OPTIONS),
                header_values(&reply, &header::CACHE_CONTROL),
                reply.text().contains("<script"),
            ),
            (
                1,
                true,
                vec!["nosniff".to_owned()],
                vec!["no-store".to_owned()],
                false
            ),
            "{csp:?}"
        );
    }

    // TC: VIEW-01 (who has no session gets no buttons)
    #[tokio::test]
    async fn shows_no_share_buttons_when_viewer_has_no_session() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), None).await;
        let body = reply.text();
        assert!(
            reply.status == StatusCode::OK
                && !has_form_posting_to(&body, &format!("/a/{id}/share"))
                && !has_form_posting_to(&body, &format!("/a/{id}/unshare")),
            "{body}"
        );
    }

    // TC: VIEW-01 (the owner of a shared artifact may unshare it)
    #[tokio::test]
    async fn shows_unshare_button_to_owner_when_shared() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let body = get(&ctx, API, &format!("/a/{id}"), Some(&cookie))
            .await
            .text();
        assert!(
            has_form_posting_to(&body, &format!("/a/{id}/unshare"))
                && !has_form_posting_to(&body, &format!("/a/{id}/share")),
            "{body}"
        );
    }

    // TC: VIEW-01 (the owner of a private artifact may share it)
    #[tokio::test]
    async fn shows_share_button_to_owner_when_private() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let body = get(&ctx, API, &format!("/a/{id}"), Some(&cookie))
            .await
            .text();
        assert!(
            has_form_posting_to(&body, &format!("/a/{id}/share"))
                && !has_form_posting_to(&body, &format!("/a/{id}/unshare")),
            "{body}"
        );
    }

    // TC: VIEW-01 (another user sees a shared artifact without buttons)
    #[tokio::test]
    async fn shows_no_share_buttons_when_viewer_is_another_user() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let carol = carol(&ctx.world).await;
        let cookie = cookie_of(&ctx.world, &carol).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), Some(&cookie)).await;
        let body = reply.text();
        assert!(
            reply.status == StatusCode::OK
                && !has_form_posting_to(&body, &format!("/a/{id}/unshare")),
            "{body}"
        );
    }
}

mod access {
    use super::*;

    // TC: VIEW-02
    #[tokio::test]
    async fn redirects_to_login_with_return_when_private_without_cookie() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), None).await;
        assert_eq!(
            (reply.status, location(&reply)),
            (
                StatusCode::FOUND,
                format!("https://liftoff.test/login?rd=%2Fa%2F{id}")
            )
        );
    }

    // TC: VIEW-02 (a file returns to its wrapper, never to the raw file)
    #[tokio::test]
    async fn redirects_to_login_returning_to_wrapper_when_private_file_without_cookie() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/index.html"), None).await;
        assert_eq!(
            (reply.status, location(&reply)),
            (
                StatusCode::FOUND,
                format!("https://liftoff.test/login?rd=%2Fa%2F{id}")
            )
        );
    }

    // TC: VIEW-02
    #[tokio::test]
    async fn serves_wrapper_when_private_with_owner_cookie() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), Some(&cookie)).await;
        let frame = iframe(&reply.text()).unwrap_or_default();
        assert_eq!(
            (
                reply.status,
                frame.contains(&format!(r#"src="/a/{id}/v/1/index.html""#))
            ),
            (StatusCode::OK, true),
            "{frame}"
        );
    }

    // TC: VIEW-02
    #[tokio::test]
    async fn serves_file_when_private_with_owner_cookie() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/index.html"), Some(&cookie)).await;
        assert_eq!(
            (reply.status, reply.text()),
            (StatusCode::OK, PAGE.to_owned())
        );
    }

    // TC: VIEW-02 (a session of someone who may not open it)
    #[tokio::test]
    async fn forbids_when_private_with_other_users_cookie() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let carol = carol(&ctx.world).await;
        let cookie = cookie_of(&ctx.world, &carol).await;
        let reply = get(&ctx, API, &format!("/a/{id}"), Some(&cookie)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
    }

    // TC: VIEW-02 (a JSON client gets 401 instead of the login page)
    #[tokio::test]
    async fn unauthorized_when_private_and_client_wants_json() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let headers = [
            (header::HOST.as_str(), API),
            (header::ACCEPT.as_str(), "application/json"),
        ];
        let reply = ctx
            .call_with("GET", &format!("/a/{id}/v/1/index.html"), &headers, None)
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
}

mod file {
    use super::*;

    // TC: VIEW-03
    #[tokio::test]
    async fn sandboxes_index_html_when_served() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/index.html"), None).await;
        assert_eq!(
            (
                reply.status,
                header_values(&reply, &header::CONTENT_SECURITY_POLICY),
                header_values(&reply, &header::X_CONTENT_TYPE_OPTIONS),
            ),
            (
                StatusCode::OK,
                vec![SANDBOX_CSP.to_owned()],
                vec!["nosniff".to_owned()]
            )
        );
    }

    // TC: VIEW-03 (the HTML of the version as it was published)
    #[tokio::test]
    async fn serves_html_of_version_when_index_html_requested() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/index.html"), None).await;
        assert_eq!(
            (content_type(&reply), reply.text()),
            ("text/html; charset=utf-8".to_owned(), PAGE.to_owned())
        );
    }

    // TC: VIEW-03 (every file of a version is sandboxed, not only the
    // HTML: an SVG or an HTML file opened on its own runs in an opaque
    // origin too)
    #[tokio::test]
    async fn sandboxes_other_files_when_served() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[("img/icon.svg", b"<svg/>")]).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/img/icon.svg"), None).await;
        assert_eq!(
            (
                header_values(&reply, &header::CONTENT_SECURITY_POLICY),
                header_values(&reply, &header::X_CONTENT_TYPE_OPTIONS),
            ),
            (vec![SANDBOX_CSP.to_owned()], vec!["nosniff".to_owned()])
        );
    }

    // TC: VIEW-04 (a file by its relative path, nested ones included)
    #[tokio::test]
    async fn serves_bytes_of_file_when_relative_path_requested() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[("css/site.css", b"p{color:red}")]).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/css/site.css"), None).await;
        assert_eq!(
            (reply.status, reply.body),
            (StatusCode::OK, b"p{color:red}".to_vec())
        );
    }

    // TC: VIEW-04
    #[tokio::test]
    async fn types_file_by_extension_when_extension_known() {
        let ctx = WorldCtx::new().await;
        let files: [(&str, &[u8]); 5] = [
            ("css/site.css", b"p{}"),
            ("app.js", b"void 0"),
            ("data.json", b"{}"),
            ("img/logo.png", b"\x89PNG"),
            ("img/icon.svg", b"<svg/>"),
        ];
        let id = shared(&ctx.world, "Report", &files).await;
        let mut types = Vec::new();
        for (name, _) in files {
            let reply = get(&ctx, API, &format!("/a/{id}/v/1/{name}"), None).await;
            types.push((name, content_type(&reply)));
        }
        assert_eq!(
            types,
            [
                ("css/site.css", "text/css; charset=utf-8".to_owned()),
                ("app.js", "text/javascript; charset=utf-8".to_owned()),
                ("data.json", "application/json".to_owned()),
                ("img/logo.png", "image/png".to_owned()),
                ("img/icon.svg", "image/svg+xml".to_owned()),
            ]
        );
    }

    // TC: VIEW-04
    #[tokio::test]
    async fn octet_stream_when_extension_unknown() {
        let ctx = WorldCtx::new().await;
        let files: [(&str, &[u8]); 2] = [("blob.xyz", b"?"), ("LICENSE", b"MIT")];
        let id = shared(&ctx.world, "Report", &files).await;
        let mut types = Vec::new();
        for (name, _) in files {
            let reply = get(&ctx, API, &format!("/a/{id}/v/1/{name}"), None).await;
            types.push((name, reply.status, content_type(&reply)));
        }
        assert_eq!(
            types,
            [
                (
                    "blob.xyz",
                    StatusCode::OK,
                    "application/octet-stream".to_owned()
                ),
                (
                    "LICENSE",
                    StatusCode::OK,
                    "application/octet-stream".to_owned()
                ),
            ]
        );
    }
}

mod missing {
    use super::*;

    // TC: VIEW-05
    #[tokio::test]
    async fn not_found_when_version_unknown() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let mut statuses = Vec::new();
        for path in [format!("/a/{id}/v/2"), format!("/a/{id}/v/2/index.html")] {
            statuses.push(get(&ctx, API, &path, None).await.status);
        }
        assert_eq!(statuses, [StatusCode::NOT_FOUND, StatusCode::NOT_FOUND]);
    }

    // TC: VIEW-05 (no version number at all)
    #[tokio::test]
    async fn not_found_when_version_is_no_number() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let mut statuses = Vec::new();
        for path in [
            format!("/a/{id}/v/0"),
            format!("/a/{id}/v/latest"),
            format!("/a/{id}/v/latest/index.html"),
        ] {
            statuses.push(get(&ctx, API, &path, None).await.status);
        }
        assert_eq!(
            statuses,
            [
                StatusCode::NOT_FOUND,
                StatusCode::NOT_FOUND,
                StatusCode::NOT_FOUND
            ]
        );
    }

    // TC: VIEW-05 (a deleted version is gone)
    #[tokio::test]
    async fn not_found_when_version_deleted() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "First draft", &[]).await;
        update(&ctx.world, id, "Second draft").await;
        ctx.world
            .artifacts()
            .delete_version(&ctx.world.owner, id, VersionNumber::FIRST)
            .await
            .unwrap();
        let mut statuses = Vec::new();
        for path in [format!("/a/{id}/v/1"), format!("/a/{id}/v/1/index.html")] {
            statuses.push(get(&ctx, API, &path, None).await.status);
        }
        assert_eq!(statuses, [StatusCode::NOT_FOUND, StatusCode::NOT_FOUND]);
    }

    // TC: VIEW-05 (a file the version does not have)
    #[tokio::test]
    async fn not_found_when_file_unknown() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[("style.css", b"p{}")]).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/missing.css"), None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }

    // TC: VIEW-05 (an unknown artifact, and a path that is no id)
    #[tokio::test]
    async fn not_found_when_artifact_unknown() {
        let ctx = WorldCtx::new().await;
        let mut statuses = Vec::new();
        for path in [
            "/a/zzzzzzzzzzzz",
            "/a/zzzzzzzzzzzz/v/1/index.html",
            "/a/nope",
        ] {
            statuses.push(get(&ctx, API, path, None).await.status);
        }
        assert_eq!(
            statuses,
            [
                StatusCode::NOT_FOUND,
                StatusCode::NOT_FOUND,
                StatusCode::NOT_FOUND
            ]
        );
    }
}

mod switcher {
    use super::*;

    /// A shared artifact with version 1 «First draft» at the world's
    /// start (2026-10-13 00:00 UTC) and version 2 «Second draft» 90
    /// minutes later.
    async fn two_versions(world: &World) -> ArtifactId {
        let id = shared(world, "First draft", &[]).await;
        world.ports.clock.advance(time::Duration::minutes(90));
        update(world, id, "Second draft").await;
        id
    }

    // TC: VIEW-06 (a plain link per version, no JavaScript)
    #[tokio::test]
    async fn links_every_version_when_two_versions() {
        let ctx = WorldCtx::new().await;
        let id = two_versions(&ctx.world).await;
        let body = get(&ctx, API, &format!("/a/{id}"), None).await.text();
        assert!(
            body.contains(&format!(r#"href="/a/{id}/v/1""#))
                && body.contains(&format!(r#"href="/a/{id}/v/2""#)),
            "{body}"
        );
    }

    // TC: VIEW-06
    #[tokio::test]
    async fn shows_date_of_every_version_when_two_versions() {
        let ctx = WorldCtx::new().await;
        let id = two_versions(&ctx.world).await;
        let body = get(&ctx, API, &format!("/a/{id}"), None).await.text();
        assert!(
            body.contains("2026-10-13 00:00 UTC") && body.contains("2026-10-13 01:30 UTC"),
            "{body}"
        );
    }

    // TC: VIEW-06 (the switcher shows the date and the title)
    #[tokio::test]
    async fn shows_title_of_every_version_when_two_versions() {
        let ctx = WorldCtx::new().await;
        let id = two_versions(&ctx.world).await;
        let body = get(&ctx, API, &format!("/a/{id}"), None).await.text();
        assert!(
            body.contains("First draft") && body.contains("Second draft"),
            "{body}"
        );
    }

    // TC: VIEW-06 (an older version: which one of how many, framed)
    #[tokio::test]
    async fn shows_older_version_of_how_many_when_version_requested() {
        let ctx = WorldCtx::new().await;
        let id = two_versions(&ctx.world).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1"), None).await;
        let body = reply.text();
        let frame = iframe(&body).unwrap_or_default();
        assert_eq!(
            (
                reply.status,
                body.to_lowercase().contains("version 1 of 2"),
                frame.contains(&format!(r#"src="/a/{id}/v/1/index.html""#)),
            ),
            (StatusCode::OK, true, true),
            "{body}"
        );
    }
}

mod cache {
    use super::*;

    // TC: VIEW-07
    #[tokio::test]
    async fn private_no_store_when_artifact_private() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/index.html"), Some(&cookie)).await;
        assert_eq!(
            header_values(&reply, &header::CACHE_CONTROL),
            ["private, no-store"]
        );
    }

    // TC: VIEW-07
    #[tokio::test]
    async fn public_max_age_when_artifact_shared() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, API, &format!("/a/{id}/v/1/index.html"), None).await;
        assert_eq!(
            header_values(&reply, &header::CACHE_CONTROL),
            ["public, max-age=300"]
        );
    }
}

mod artifacts_host {
    use super::*;

    /// The API of an installation with `[artifacts] host = "artifacts.test"`.
    async fn ctx() -> WorldCtx {
        WorldCtx::with_settings(|settings| {
            settings.artifacts.host = Some(Host::new(ARTIFACTS).unwrap());
        })
        .await
    }

    // TC: VIEW-08 (`https://<artifacts.host>/<id>`)
    #[tokio::test]
    async fn serves_wrapper_at_root_of_artifacts_host_when_set() {
        let ctx = ctx().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, ARTIFACTS, &format!("/{id}"), None).await;
        let frame = iframe(&reply.text()).unwrap_or_default();
        assert_eq!(
            (
                reply.status,
                frame.contains(IFRAME_SANDBOX)
                    && frame.contains(&format!(r#"src="/{id}/v/1/index.html""#))
            ),
            (StatusCode::OK, true),
            "{frame}"
        );
    }

    // TC: VIEW-08
    #[tokio::test]
    async fn serves_sandboxed_file_on_artifacts_host_when_set() {
        let ctx = ctx().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, ARTIFACTS, &format!("/{id}/v/1/index.html"), None).await;
        assert_eq!(
            (
                reply.status,
                header_values(&reply, &header::CONTENT_SECURITY_POLICY),
                reply.text()
            ),
            (
                StatusCode::OK,
                vec![SANDBOX_CSP.to_owned()],
                PAGE.to_owned()
            )
        );
    }

    // TC: VIEW-08
    #[tokio::test]
    async fn not_found_under_a_on_api_host_when_artifacts_host_set() {
        let ctx = ctx().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let mut statuses = Vec::new();
        for path in [format!("/a/{id}"), format!("/a/{id}/v/1/index.html")] {
            statuses.push(get(&ctx, API, &path, None).await.status);
        }
        assert_eq!(statuses, [StatusCode::NOT_FOUND, StatusCode::NOT_FOUND]);
    }

    // TC: VIEW-08 (the artifacts' host serves the artifacts alone: the
    // API and the daemon's pages stay on the API host)
    #[tokio::test]
    async fn not_found_for_api_and_pages_on_artifacts_host_when_set() {
        let ctx = ctx().await;
        let mut statuses = Vec::new();
        for path in ["/api/v1/server", "/login", "/gate/start"] {
            statuses.push(get(&ctx, ARTIFACTS, path, None).await.status);
        }
        assert_eq!(
            statuses,
            [
                StatusCode::NOT_FOUND,
                StatusCode::NOT_FOUND,
                StatusCode::NOT_FOUND
            ]
        );
    }

    // TC: VIEW-02, VIEW-08 (the API host's cookie never reaches the
    // artifacts' host: a browser without a session there goes through
    // the gate's handover of the API host's session, as a private
    // address of an environment)
    #[tokio::test]
    async fn redirects_to_gate_start_when_private_without_session_on_artifacts_host() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let reply = get(&ctx, ARTIFACTS, &format!("/{id}/v/1/index.html"), None).await;
        assert_eq!(
            (reply.status, location(&reply)),
            (
                StatusCode::FOUND,
                format!("https://liftoff.test/gate/start?host=artifacts.test&rd=%2F{id}")
            )
        );
    }

    // TC: VIEW-02, VIEW-08 (no `browser` session on the API host yet:
    // the login page, returning to the handover)
    #[tokio::test]
    async fn redirects_to_login_page_when_gate_start_for_artifacts_host_without_session() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let reply = get(
            &ctx,
            API,
            &format!("/gate/start?host=artifacts.test&rd=%2F{id}"),
            None,
        )
        .await;
        assert_eq!(
            (reply.status, location(&reply)),
            (
                StatusCode::FOUND,
                format!("https://liftoff.test/login?host=artifacts.test&rd=%2F{id}")
            )
        );
    }

    // TC: VIEW-08 (the API host's `browser` session is handed over to
    // the artifacts' host by a one-time code)
    #[tokio::test]
    async fn redirects_to_callback_on_artifacts_host_when_gate_start_with_browser_session() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = get(
            &ctx,
            API,
            &format!("/gate/start?host=artifacts.test&rd=%2F{id}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                location(&reply).starts_with("https://artifacts.test/.liftoff/callback?code=lfc_")
            ),
            (StatusCode::FOUND, true),
            "{:?}",
            reply.headers
        );
    }

    // TC: VIEW-08 (the callback on the artifacts' host sets its own
    // session and returns to the wrapper)
    #[tokio::test]
    async fn sets_host_session_and_returns_when_callback_redeemed_on_artifacts_host() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let start = get(
            &ctx,
            API,
            &format!("/gate/start?host=artifacts.test&rd=%2F{id}"),
            Some(&cookie),
        )
        .await;
        let callback = location(&start);
        let path = callback
            .strip_prefix("https://artifacts.test")
            .unwrap_or_default();
        let reply = get(&ctx, ARTIFACTS, path, None).await;
        let set_cookie = header_values(&reply, &header::SET_COOKIE);
        assert_eq!(
            (
                reply.status,
                location(&reply),
                set_cookie.len(),
                set_cookie.iter().all(|line| {
                    line.starts_with("__Host-liftoff_gate=")
                        && line.contains("; Secure")
                        && line.contains("; HttpOnly")
                })
            ),
            (StatusCode::FOUND, format!("/{id}"), 1, true),
            "{set_cookie:?}"
        );
    }

    // TC: VIEW-02, VIEW-08 (a private artifact opens on the artifacts'
    // host with its owner's session there)
    #[tokio::test]
    async fn serves_private_wrapper_when_owners_session_on_artifacts_host() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = artifacts_cookie_of(&ctx, &ctx.world.owner, id).await;
        let reply = get(&ctx, ARTIFACTS, &format!("/{id}"), Some(&cookie)).await;
        let frame = iframe(&reply.text()).unwrap_or_default();
        assert_eq!(
            (
                reply.status,
                frame.contains(&format!(r#"src="/{id}/v/1/index.html""#))
            ),
            (StatusCode::OK, true),
            "{cookie} {frame}"
        );
    }

    // TC: VIEW-02, VIEW-08 (someone else's session there is refused)
    #[tokio::test]
    async fn forbids_private_when_other_users_session_on_artifacts_host() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let carol = carol(&ctx.world).await;
        let cookie = artifacts_cookie_of(&ctx, &carol, id).await;
        let reply = get(&ctx, ARTIFACTS, &format!("/{id}"), Some(&cookie)).await;
        assert_eq!(
            (reply.status, cookie.starts_with("__Host-liftoff_gate=")),
            (StatusCode::FORBIDDEN, true)
        );
    }

    // TC: ART-06, VIEW-08 (the buttons on the artifacts' host: its own
    // origin, back to its wrapper)
    #[tokio::test]
    async fn shares_and_returns_to_wrapper_when_owner_posts_from_artifacts_host() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = artifacts_cookie_of(&ctx, &ctx.world.owner, id).await;
        let reply = post_form(
            &ctx,
            ARTIFACTS,
            &format!("/{id}/share"),
            Some("https://artifacts.test"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                location(&reply),
                visibility_of(&ctx.world, id).await
            ),
            (StatusCode::SEE_OTHER, format!("/{id}"), Visibility::Shared)
        );
    }

    // TC: ART-06, VIEW-08 (the form must come from the viewer's own host, which is the artifacts' host, not the API host)
    #[tokio::test]
    async fn refuses_share_when_origin_is_api_host_and_artifacts_host_set() {
        let ctx = ctx().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = artifacts_cookie_of(&ctx, &ctx.world.owner, id).await;
        let reply = post_form(
            &ctx,
            ARTIFACTS,
            &format!("/{id}/share"),
            Some("https://liftoff.test"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (reply.status, visibility_of(&ctx.world, id).await),
            (StatusCode::FORBIDDEN, Visibility::Private)
        );
    }

    /// What a `fmt` subscriber wrote, shared with the test.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned()
        }
    }

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The route of a link as the request span logs it.
    const LINK_ROUTE: &str = "/api/v1/auth/links/{token}";

    // TC: OTL-06, VIEW-08 (with the artifacts' host set, the request's span still names the matched route, not the path that carries a token)
    #[tokio::test]
    async fn span_names_matched_route_when_artifacts_host_set() {
        let ctx = ctx().await;
        let token = "lfl_0123456789abcdefghijklmnopqrstuvwxyzABCD";
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let _status = {
            let _default = tracing::subscriber::set_default(subscriber);
            get(&ctx, API, &format!("/api/v1/auth/links/{token}"), None)
                .await
                .status
        };
        let log = captured.text();
        assert_eq!(
            (log.contains(LINK_ROUTE), log.contains(token)),
            (true, false),
            "{log}"
        );
    }
}

mod buttons {
    use super::*;

    // TC: ART-06 (the wrapper's share button: back to the wrapper)
    #[tokio::test]
    async fn shares_and_returns_to_wrapper_when_owner_posts_share() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = post_form(
            &ctx,
            API,
            &format!("/a/{id}/share"),
            Some("https://liftoff.test"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                location(&reply),
                visibility_of(&ctx.world, id).await
            ),
            (
                StatusCode::SEE_OTHER,
                format!("/a/{id}"),
                Visibility::Shared
            )
        );
    }

    // TC: ART-06 (the wrapper's unshare button)
    #[tokio::test]
    async fn unshares_and_returns_to_wrapper_when_owner_posts_unshare() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = post_form(
            &ctx,
            API,
            &format!("/a/{id}/unshare"),
            Some("https://liftoff.test"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                location(&reply),
                visibility_of(&ctx.world, id).await
            ),
            (
                StatusCode::SEE_OTHER,
                format!("/a/{id}"),
                Visibility::Private
            )
        );
    }

    // TC: ART-06 (a form of another site does not act with the owner's cookie)
    #[tokio::test]
    async fn refuses_share_when_form_posted_from_another_origin() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = post_form(
            &ctx,
            API,
            &format!("/a/{id}/share"),
            Some("https://evil.test"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (reply.status, visibility_of(&ctx.world, id).await),
            (StatusCode::FORBIDDEN, Visibility::Private)
        );
    }

    // TC: ART-06, ART-11 (someone else's cookie changes nothing)
    #[tokio::test]
    async fn refuses_share_when_poster_is_another_user() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let carol = carol(&ctx.world).await;
        let cookie = cookie_of(&ctx.world, &carol).await;
        let reply = post_form(
            &ctx,
            API,
            &format!("/a/{id}/share"),
            Some("https://liftoff.test"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (reply.status, visibility_of(&ctx.world, id).await),
            (StatusCode::FORBIDDEN, Visibility::Private)
        );
    }

    // TC: ART-06, ART-11 (a shared artifact lets anybody in, but only its
    // owner or an admin may unshare it: the core refuses the button)
    #[tokio::test]
    async fn refuses_unshare_when_poster_is_another_user_and_artifact_shared() {
        let ctx = WorldCtx::new().await;
        let id = shared(&ctx.world, "Report", &[]).await;
        let carol = carol(&ctx.world).await;
        let cookie = cookie_of(&ctx.world, &carol).await;
        let reply = post_form(
            &ctx,
            API,
            &format!("/a/{id}/unshare"),
            Some("https://liftoff.test"),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            (reply.status, visibility_of(&ctx.world, id).await),
            (StatusCode::FORBIDDEN, Visibility::Shared)
        );
    }

    // TC: ART-06 (without a session the button goes to the login, as
    // the wrapper)
    #[tokio::test]
    async fn redirects_to_login_when_form_posted_without_session() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let reply = post_form(
            &ctx,
            API,
            &format!("/a/{id}/share"),
            Some("https://liftoff.test"),
            None,
        )
        .await;
        assert_eq!(
            (
                reply.status,
                location(&reply),
                visibility_of(&ctx.world, id).await
            ),
            (
                StatusCode::FOUND,
                format!("https://liftoff.test/login?rd=%2Fa%2F{id}"),
                Visibility::Private
            )
        );
    }

    // TC: ART-06 (a form without `Origin` is no form of this site)
    #[tokio::test]
    async fn refuses_share_when_form_has_no_origin() {
        let ctx = WorldCtx::new().await;
        let id = publish(&ctx.world, "Report", &[]).await;
        let cookie = cookie_of(&ctx.world, &ctx.world.owner).await;
        let reply = post_form(&ctx, API, &format!("/a/{id}/share"), None, Some(&cookie)).await;
        assert_eq!(
            (reply.status, visibility_of(&ctx.world, id).await),
            (StatusCode::FORBIDDEN, Visibility::Private)
        );
    }
}
