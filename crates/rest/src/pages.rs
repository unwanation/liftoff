//! The daemon's own pages.
//!
//! The browser without OIDC: embedded HTML with a little
//! inline CSS and no JavaScript, served with the v1 headers (`nosniff`,
//! [`CSP`], `no-store`).
//!
//! - [`login`]: the login page of the API host without a module that
//!   replaces it (`render_page(login)`): a field to paste the one-time
//!   link and the text to run `lof dev` or `lof artifact open`;
//! - [`offline`]: a tunnel whose `lof dev` is not connected (`503`);
//! - [`artifact`]: the wrapper of an artifact's version, with its own stylesheet and [`ARTIFACT_CSP`];
//! - [`forbidden`]: a session of someone who may not open the address
//!   (`403`).
//!
//! The function and tunnel listeners (`crates/liftoffd`) render
//! [`offline`] and [`forbidden`] too.

mod artifact;
mod forbidden;
mod login;
mod offline;

pub use artifact::{ARTIFACT_CSP, ARTIFACT_STYLE, ArtifactPage, artifact};
pub use forbidden::forbidden;
pub use login::login;
pub use offline::offline;

/// The one stylesheet of every page, inlined in `<style>`; [`CSP`] allows
/// it by its hash and nothing else.
pub const STYLE: &str = "body{font:16px/1.5 system-ui,sans-serif;max-width:36rem;margin:3rem auto;padding:0 1rem;color:#1d1d1f;background:#fff}h1{font-size:1.5rem}code{font-size:.95em;background:#f0f0f2;padding:0 .25rem;border-radius:3px}label{display:block;margin-top:1rem}input{display:block;width:100%;box-sizing:border-box;padding:.5rem;margin:.25rem 0;font:inherit}button{padding:.5rem 1rem;font:inherit}.notice{padding:.5rem .75rem;border-left:3px solid #b42318;background:#fdf0ef}@media (prefers-color-scheme:dark){body{color:#e6e6e8;background:#161618}code{background:#2a2a2e}.notice{background:#3a1d1b}}";

/// The `Content-Security-Policy` of a page.
///
/// Nothing but [`STYLE`] (by its SHA-256), no frames, no `<base>`.
/// `form-action` stays unset: the login
/// form answers with a redirect to the target's host, which a
/// `form-action 'self'` would block.
pub const CSP: &str = "default-src 'none'; style-src 'sha256-o03eQqu7FFd63ie87eyXhBqzvs15zg3SRuTZAaUGZZc='; frame-ancestors 'none'; base-uri 'none'";

/// A whole page titled `title` around `body` (HTML already escaped).
fn document(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <title>{title} · liftoff</title>\n<style>{STYLE}</style>\n</head>\n<body>\n{body}</body>\n</html>\n"
    )
}

/// `text` safe inside an element or a quoted attribute.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for char in text.chars() {
        match char {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    use super::*;

    mod csp {
        use super::*;

        // TC: OTL-08 (the CSP allows the inline stylesheet by its hash:
        // a changed STYLE needs the new hash)
        #[test]
        fn allows_style_by_its_hash_when_page_rendered() {
            let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(STYLE));
            assert!(
                CSP.contains(&format!("style-src 'sha256-{hash}'")),
                "sha256-{hash}"
            );
        }

        // TC: OTL-08 (nothing else loads: no script at all)
        #[test]
        fn allows_nothing_by_default_when_page_rendered() {
            assert!(CSP.starts_with("default-src 'none';") && !CSP.contains("script"));
        }
    }

    mod escape {
        use super::*;

        // TC: OTL-08
        #[test]
        fn escapes_markup_and_quotes_when_text_has_them() {
            assert_eq!(
                escape(r#"/"><script>'&"#),
                "/&quot;&gt;&lt;script&gt;&#39;&amp;"
            );
        }
    }

    mod document {
        use super::*;

        // TC: OTL-08 (the stylesheet the hash allows is the one inlined)
        #[test]
        fn inlines_style_exactly_when_rendered() {
            assert!(document("t", "").contains(&format!("<style>{STYLE}</style>")));
        }
    }
}
