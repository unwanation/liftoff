//! `artifact publish|update|share|unshare|open|ls|versions|rollback|rm`.
//!
//! `publish` and `update` send the page and its files as
//! `multipart/form-data` (`html`, a `files` field per file whose
//! `filename` is its name in the artifact, `title`). The files are read
//! relative to the current directory before anything is sent: a bare
//! path inside it is named by its path from there (`./img/a.png` is
//! `img/a.png`), one outside it by its base name (`../shared/logo.png` is
//! `logo.png`), `name=path` by `name`. `open` prints a fresh one-time
//! link; with `--browser` it also hands the link to the browser of this
//! machine (`xdg-open`, `open` on macOS), whose failure is a warning.

use std::path::{Component, Path, PathBuf};

use liftoff_api::{ArtifactRollback, ArtifactVersionView, ArtifactView, LinkCreate, LinkIssued};
use liftoff_core::domain::{ArtifactPath, BlobHash};
use reqwest::Method;

use super::{Ctx, parse, segment};
use crate::args::ArtifactCommand;
use crate::client::Client;
use crate::error::CliError;
use crate::{Sleeper, render};

/// The program that opens a URL in the browser of this machine.
const BROWSER: &str = if cfg!(target_os = "macos") {
    "open"
} else {
    "xdg-open"
};

/// A version as read from disk: the page, its files by their names in
/// the artifact, and a title.
struct Version {
    html: Vec<u8>,
    files: Vec<(String, Vec<u8>)>,
    title: Option<String>,
}

/// Runs `artifact …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: ArtifactCommand,
) -> Result<(), CliError> {
    match command {
        ArtifactCommand::Publish { html, files, title } => {
            publish(ctx, None, html, &files, title).await
        }
        ArtifactCommand::Update {
            id,
            html,
            files,
            title,
        } => {
            let id = ctx.text(id, "<id>", "Artifact")?;
            publish(ctx, Some(id), html, &files, title).await
        }
        ArtifactCommand::Share { id } => visibility(ctx, id, true).await,
        ArtifactCommand::Unshare { id } => visibility(ctx, id, false).await,
        ArtifactCommand::Open { id, browser } => open(ctx, id, browser).await,
        ArtifactCommand::Ls { all } => {
            ctx.finish()?;
            let query = if all {
                vec![("all", "true".to_owned())]
            } else {
                Vec::new()
            };
            let reply = ctx.client()?.get("/artifacts", &query).await?;
            ctx.show(&reply, |body| {
                Ok(artifacts(&parse::<Vec<ArtifactView>>(body)?))
            })
        }
        ArtifactCommand::Versions { id } => {
            let id = ctx.text(id, "<id>", "Artifact")?;
            ctx.finish()?;
            let reply = ctx.client()?.get(&path(&id, "/versions"), &[]).await?;
            ctx.show(&reply, |body| {
                Ok(versions(&parse::<Vec<ArtifactVersionView>>(body)?))
            })
        }
        ArtifactCommand::Rollback { id, to } => rollback(ctx, id, to).await,
        ArtifactCommand::Rm { id, version } => rm(ctx, id, version).await,
    }
}

/// `publish <html>` (`id` is `None`) and `update <id> <html>`: the page
/// and its files, read before anything is sent.
async fn publish<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    id: Option<String>,
    html: Option<String>,
    files: &[String],
    title: Option<String>,
) -> Result<(), CliError> {
    let html = ctx.text(html, "<html>", "HTML file")?;
    ctx.finish()?;
    let client = ctx.client()?;
    let version = read_version(&ctx.env.cwd, &html, files, title)?;
    let Some(id) = id else {
        let reply = upload(&client, Method::POST, "/artifacts", &version).await?;
        return ctx.show(&reply, |body| {
            let artifact: ArtifactView = parse(body)?;
            Ok(artifact.login_url.map_or_else(
                || format!("{}\n", artifact.url),
                |link| format!("{}\n{link}\n", artifact.url),
            ))
        });
    };
    let reply = upload(&client, Method::PUT, &path(&id, ""), &version).await?;
    ctx.show(&reply, |body| {
        Ok(format!("{}\n", parse::<ArtifactView>(body)?.url))
    })
}

/// `share <id>` (`shared`) and `unshare <id>`.
async fn visibility<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    id: Option<String>,
    shared: bool,
) -> Result<(), CliError> {
    let (question, route) = if shared {
        ("Artifact to share", "/share")
    } else {
        ("Artifact to make private", "/unshare")
    };
    let id = ctx.text(id, "<id>", question)?;
    ctx.finish()?;
    let reply = ctx.client()?.post(&path(&id, route)).await?;
    ctx.show(&reply, |body| {
        let artifact: ArtifactView = parse(body)?;
        Ok(match artifact.expires_at {
            None => format!("{} is shared with anybody: {}\n", artifact.id, artifact.url),
            Some(at) => format!("{} is private until {at}\n", artifact.id),
        })
    })
}

/// `rollback <id> --to N`.
async fn rollback<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    id: Option<String>,
    to: Option<u32>,
) -> Result<(), CliError> {
    let id = ctx.text(id, "<id>", "Artifact")?;
    let to = ctx.parsed(to, "--to", "Version to copy as the current one")?;
    ctx.finish()?;
    let reply = ctx
        .client()?
        .send_json(
            Method::POST,
            &path(&id, "/rollback"),
            &[],
            &ArtifactRollback { to },
        )
        .await?;
    ctx.show(&reply, |body| {
        let artifact: ArtifactView = parse(body)?;
        Ok(format!(
            "{} is now version {} (a copy of version {to})\n",
            artifact.id, artifact.version
        ))
    })
}

/// `rm <id> [--version N]`.
async fn rm<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    id: Option<String>,
    version: Option<u32>,
) -> Result<(), CliError> {
    let id = ctx.text(id, "<id>", "Artifact to remove")?;
    ctx.finish()?;
    let client = ctx.client()?;
    match version {
        None => {
            let reply = client.delete(&path(&id, ""), &[]).await?;
            ctx.ack(&reply, &format!("removed {id}"))
        }
        Some(n) => {
            let reply = client
                .delete(&path(&id, &format!("/versions/{n}")), &[])
                .await?;
            ctx.ack(&reply, &format!("removed version {n} of {id}"))
        }
    }
}

/// `open <id> [--browser]`: a fresh one-time link to the artifact,
/// printed, then handed to the browser when asked.
async fn open<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    id: Option<String>,
    browser: bool,
) -> Result<(), CliError> {
    let id = ctx.text(id, "<id>", "Artifact to open")?;
    ctx.finish()?;
    let body = LinkCreate {
        target: format!("artifact:{id}"),
    };
    let reply = ctx
        .client()?
        .send_json(Method::POST, "/auth/links", &[], &body)
        .await?;
    let link: LinkIssued = parse(&reply.body)?;
    ctx.show(&reply, |_| Ok(format!("{}\n", link.url)))?;
    if browser && let Err(error) = ctx.env.host.run(BROWSER, &[link.url.as_str()], None) {
        ctx.tell(&format!(
            "warning: cannot open a browser ({error}); open the link above yourself\n"
        ))?;
    }
    Ok(())
}

/// `/artifacts/<id><rest>`.
fn path(id: &str, rest: &str) -> String {
    format!("/artifacts/{}{rest}", segment(id))
}

/// Sends `version` as `method path` in `multipart/form-data`.
async fn upload(
    client: &Client,
    method: Method,
    path: &str,
    version: &Version,
) -> Result<crate::client::Reply, CliError> {
    let (content_type, body) = form(version);
    client
        .send_body(method, path, &content_type, reqwest::Body::from(body))
        .await
}

/// The `multipart/form-data` of `version` and its content type. The
/// boundary is made of the hash of every byte it carries, which none of
/// them can contain.
fn form(version: &Version) -> (String, Vec<u8>) {
    let mut everything = version.html.clone();
    for (name, bytes) in &version.files {
        everything.extend_from_slice(name.as_bytes());
        everything.extend_from_slice(bytes);
    }
    if let Some(title) = &version.title {
        everything.extend_from_slice(title.as_bytes());
    }
    let boundary = format!("liftoff-{}", BlobHash::of(&everything));
    drop(everything);
    let mut body = Vec::new();
    let mut part = |head: String, bytes: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\n{head}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    };
    part(
        "Content-Disposition: form-data; name=\"html\"; filename=\"index.html\"\r\n\
         Content-Type: text/html; charset=utf-8"
            .to_owned(),
        &version.html,
    );
    if let Some(title) = &version.title {
        part(
            "Content-Disposition: form-data; name=\"title\"".to_owned(),
            title.as_bytes(),
        );
    }
    for (name, bytes) in &version.files {
        part(
            format!(
                "Content-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\n\
                 Content-Type: application/octet-stream"
            ),
            bytes,
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Reads the page `html` and the files of `files` relative to `cwd`
/// (see the [module documentation](self)): every one, or the error that
/// names the first that cannot be read or named.
fn read_version(
    cwd: &Path,
    html: &str,
    files: &[String],
    title: Option<String>,
) -> Result<Version, CliError> {
    let html = read(cwd, html)?;
    let mut read_files = Vec::with_capacity(files.len());
    for item in files.iter().filter(|item| !item.is_empty()) {
        let (name, file) = match item.split_once('=') {
            Some((name, file)) => (name.to_owned(), file),
            None => (name_of(cwd, item), item.as_str()),
        };
        let name = checked_name(&name, item)?;
        read_files.push((name, read(cwd, file)?));
    }
    Ok(Version {
        html,
        files: read_files,
        title,
    })
}

/// The bytes of `file` relative to `cwd`.
fn read(cwd: &Path, file: &str) -> Result<Vec<u8>, CliError> {
    std::fs::read(cwd.join(file))
        .map_err(|error| CliError::Local(format!("cannot read {file}: {error}")))
}

/// The name in the artifact of the bare path `file`: its path from `cwd`
/// when it lies inside, else its base name.
fn name_of(cwd: &Path, file: &str) -> String {
    let full = normalised(&cwd.join(file));
    let inside = full
        .strip_prefix(normalised(cwd))
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty());
    inside.map_or_else(
        || {
            full.file_name().map_or_else(
                || file.to_owned(),
                |base| base.to_string_lossy().into_owned(),
            )
        },
        |relative| {
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        },
    )
}

/// `path` with `.` dropped and `..` taken back, without the file system.
fn normalised(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _popped = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `name` once it is a name a version may carry (the server's own rule,
/// checked before anything is read or sent) that fits the header of its
/// multipart field.
fn checked_name(name: &str, item: &str) -> Result<String, CliError> {
    if name.contains(['"', '\r', '\n', '\\']) {
        return Err(CliError::Usage(format!(
            "--files {item}: a name in an artifact holds no quote, backslash or line break"
        )));
    }
    ArtifactPath::new(name)
        .map(|path| path.as_str().to_owned())
        .map_err(|error| CliError::Usage(format!("--files {item}: {error}")))
}

/// `ls` as a table.
fn artifacts(artifacts: &[ArtifactView]) -> String {
    let rows: Vec<Vec<String>> = artifacts
        .iter()
        .map(|artifact| {
            vec![
                artifact.id.clone(),
                render::printable(&artifact.title),
                (if artifact.shared { "shared" } else { "private" }).to_owned(),
                artifact.version.to_string(),
                artifact
                    .expires_at
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
            ]
        })
        .collect();
    render::table(&["ID", "TITLE", "VISIBILITY", "VERSION", "EXPIRES"], &rows)
}

/// `versions` as a table, oldest first as received.
fn versions(versions: &[ArtifactVersionView]) -> String {
    let rows: Vec<Vec<String>> = versions
        .iter()
        .map(|version| {
            vec![
                version.n.to_string(),
                render::printable(&version.title),
                version.size.to_string(),
                version.created_at.clone(),
            ]
        })
        .collect();
    render::table(&["VERSION", "TITLE", "SIZE", "CREATED"], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod name_of {
        use super::*;

        // TC: CLIA-01 (a path inside the working directory keeps its
        // directories, without `./` or `a/..`)
        #[test]
        fn keeps_relative_path_when_inside() {
            assert_eq!(
                name_of(Path::new("/w/site"), "./img/../img/a.png"),
                "img/a.png"
            );
        }

        // TC: CLIA-01 (outside it, the base name)
        #[test]
        fn takes_base_name_when_outside() {
            assert_eq!(
                name_of(Path::new("/w/site"), "../shared/logo.png"),
                "logo.png"
            );
        }

        // TC: CLIA-01 (an absolute path inside it is relative to it)
        #[test]
        fn relative_to_cwd_when_absolute_inside() {
            assert_eq!(
                name_of(Path::new("/w/site"), "/w/site/css/a.css"),
                "css/a.css"
            );
        }
    }

    mod checked_name {
        use super::*;

        // TC: CLIA-01 (a name that breaks the field's header is refused
        // before anything is sent)
        #[test]
        fn refuses_quote_when_named() {
            assert!(checked_name("a\".css", "a\".css=x").is_err());
        }
    }
}
