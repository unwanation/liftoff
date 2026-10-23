//! The wrapper page of an artifact: a top bar with the title, the version shown of how many, the
//! switcher of every version with its date and title (plain links, no
//! JavaScript), the owner's share and unshare buttons (forms posting to
//! `…/share` and `…/unshare` of the viewer), a link to the raw HTML, and
//! the version's `index.html` in `<iframe sandbox="allow-scripts
//! allow-forms allow-popups allow-modals">`. The artifact's HTML is never
//! inlined: it runs in an opaque origin, never in the API's.

use core::fmt::Write as _;

use liftoff_core::domain::{ArtifactVersion, Timestamp, VersionNumber, Visibility};
use liftoff_core::service::ArtifactView;
use time::UtcOffset;

use super::escape;

/// The stylesheet of the wrapper, inlined in `<style>`; [`ARTIFACT_CSP`]
/// allows it by its hash and nothing else.
pub const ARTIFACT_STYLE: &str = "html,body{height:100%;margin:0}body{display:flex;flex-direction:column;font:14px/1.4 system-ui,sans-serif;color:#1d1d1f;background:#fff}header{display:flex;flex-wrap:wrap;align-items:center;gap:.25rem 1rem;padding:.5rem 1rem;border-bottom:1px solid #d8d8dc}h1{font-size:1rem;margin:0}.muted{color:#6e6e73}details{position:relative}summary{cursor:pointer}ol{position:absolute;z-index:1;margin:.25rem 0 0;padding:.5rem 1rem .5rem 2rem;background:#fff;border:1px solid #d8d8dc;border-radius:4px;white-space:nowrap}a{color:#0b57d0}form{margin:0}button{font:inherit;padding:.15rem .75rem}iframe{flex:1;width:100%;border:0}@media (prefers-color-scheme:dark){body,ol{color:#e6e6e8;background:#161618}header,ol{border-color:#3a3a3e}.muted{color:#98989d}a{color:#8ab4f8}}";

/// The `Content-Security-Policy` of the wrapper.
///
/// The pages' rules ([`CSP`](super::CSP)) with [`ARTIFACT_STYLE`] by its
/// SHA-256 and frames of its own origin (the version's files), which
/// `default-src 'none'` would block. No script at all.
pub const ARTIFACT_CSP: &str = "default-src 'none'; style-src 'sha256-uyAIrzwJseLx1+qR3ebIuXDUyPLEpOMBBkl0pySN1Yw='; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'";

/// The `sandbox` of the iframe: the same rights as the CSP of the files.
const SANDBOX: &str = "allow-scripts allow-forms allow-popups allow-modals";

/// What [`artifact`] renders.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactPage<'a> {
    /// The artifact and its versions.
    pub view: &'a ArtifactView,
    /// The version the page shows (the current one for `/a/{id}`).
    pub shown: VersionNumber,
    /// The path of the artifact on its host: `/a/<id>` on the API host,
    /// `/<id>` on `[artifacts] host`.
    pub base: &'a str,
}

/// The wrapper page of `page.shown`: the top bar and the sandboxed
/// iframe of; the artifact's HTML is never inlined.
#[must_use]
pub fn artifact(page: &ArtifactPage<'_>) -> String {
    let view = page.view;
    let base = escape(page.base);
    let shown = page.shown;
    let title = view
        .versions
        .iter()
        .find(|version| version.number == shown)
        .map_or(view.artifact.title.as_str(), |version| {
            version.title.as_str()
        });
    let title = escape(title);
    let mut bar = String::new();
    let _written = writeln!(bar, "<h1>{title}</h1>");
    let _written = writeln!(
        bar,
        "<span class=\"muted\">Version {shown} of {}</span>",
        view.versions.len()
    );
    bar.push_str("<details>\n<summary>Versions</summary>\n<ol>\n");
    for version in &view.versions {
        bar.push_str(&switcher_item(&base, version, shown));
    }
    bar.push_str("</ol>\n</details>\n");
    let _written = writeln!(bar, "<a href=\"{base}/v/{shown}/index.html\">Raw HTML</a>");
    let shared = view.artifact.visibility == Visibility::Shared;
    let _written = writeln!(
        bar,
        "<span class=\"muted\">{}</span>",
        if shared { "shared" } else { "private" }
    );
    if page.view.may_change_visibility {
        let (action, label) = if shared {
            ("unshare", "Unshare")
        } else {
            ("share", "Share")
        };
        let _written = writeln!(
            bar,
            "<form method=\"post\" action=\"{base}/{action}\"><button type=\"submit\">{label}</button></form>"
        );
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <title>{title} · liftoff</title>\n<style>{ARTIFACT_STYLE}</style>\n</head>\n<body>\n\
         <header>\n{bar}</header>\n\
         <iframe sandbox=\"{SANDBOX}\" src=\"{base}/v/{shown}/index.html\" title=\"{title}\"></iframe>\n\
         </body>\n</html>\n"
    )
}

/// One line of the switcher: a link to the wrapper of `version` with its
/// number, title and date; the version shown is marked.
fn switcher_item(base: &str, version: &ArtifactVersion, shown: VersionNumber) -> String {
    let number = version.number;
    let current = if number == shown {
        " aria-current=\"page\""
    } else {
        ""
    };
    format!(
        "<li><a href=\"{base}/v/{number}\"{current}>v{number}</a> {} <span class=\"muted\">{}</span></li>\n",
        escape(&version.title),
        date(version.created_at)
    )
}

/// `2026-10-13 01:30 UTC`.
fn date(at: Timestamp) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    use super::*;

    mod csp {
        use super::*;

        // TC: VIEW-01 (the CSP allows the wrapper's stylesheet by its
        // hash: a changed ARTIFACT_STYLE needs the new hash)
        #[test]
        fn allows_style_by_its_hash_when_page_rendered() {
            let hash =
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(ARTIFACT_STYLE));
            assert!(
                ARTIFACT_CSP.contains(&format!("style-src 'sha256-{hash}'")),
                "sha256-{hash}"
            );
        }
    }

    mod date {
        use super::*;

        // TC: VIEW-06
        #[test]
        fn renders_minutes_in_utc_when_offset_differs() {
            let moscow = UtcOffset::from_hms(3, 0, 0).unwrap();
            let at = Timestamp::from_unix_timestamp(1_791_855_000)
                .unwrap()
                .to_offset(moscow);
            assert_eq!(date(at), "2026-10-13 01:30 UTC");
        }
    }
}
