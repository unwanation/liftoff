//! The login page without the `oidc` module.

use core::fmt::Write as _;

use liftoff_core::domain::{Host, ReturnPath};

use super::{document, escape};

/// The login page for the private address `host`, with `notice` (safe to
/// show; escaped) above the form when there is one.
///
/// `host` is given when the browser came from one, to return to `rd`
/// there. The page is a form that sends the pasted
/// one-time link to `GET /login?link=…&host=…&rd=…`, and the text to run
/// `lof expose` or `lof artifact open` for a link. `host` and `rd` are
/// escaped.
#[must_use]
pub fn login(host: Option<&Host>, rd: &ReturnPath, notice: Option<&str>) -> String {
    let mut body = String::from("<h1>Sign in to liftoff</h1>\n");
    if let Some(notice) = notice {
        let _written = writeln!(body, "<p class=\"notice\">{}</p>", escape(notice));
    }
    if let Some(host) = host {
        let _written = writeln!(
            body,
            "<p>To open <code>{}</code>, paste a one-time link.</p>",
            escape(host.as_str())
        );
    }
    body.push_str(
        "<form method=\"get\" action=\"/login\">\n\
         <label for=\"link\">One-time link</label>\n\
         <input id=\"link\" name=\"link\" type=\"text\" required autocomplete=\"off\" \
         spellcheck=\"false\" placeholder=\"https://…/api/v1/auth/links/lfl_…\">\n",
    );
    if let Some(host) = host {
        let _written = writeln!(
            body,
            "<input type=\"hidden\" name=\"host\" value=\"{}\">",
            escape(host.as_str())
        );
    }
    let _written = writeln!(
        body,
        "<input type=\"hidden\" name=\"rd\" value=\"{}\">",
        escape(rd.as_str())
    );
    body.push_str(
        "<button type=\"submit\">Sign in</button>\n</form>\n\
         <p>No link? <code>lof expose</code> prints one for its address, \
         <code>lof artifact open &lt;id&gt;</code> opens one for an artifact. \
         A link works once, within ten minutes.</p>\n",
    );
    document("Sign in", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod login {
        use super::*;

        // TC: OTL-08 (without an address the form has no host field)
        #[test]
        fn has_no_host_field_when_no_address() {
            let page = login(None, &ReturnPath::root(), None);
            assert_eq!(
                (
                    page.contains("name=\"host\""),
                    page.contains("name=\"link\"")
                ),
                (false, true)
            );
        }

        // TC: OTL-08 (a notice is shown, escaped)
        #[test]
        fn shows_escaped_notice_when_given() {
            let page = login(None, &ReturnPath::root(), Some("<used>"));
            assert!(page.contains("<p class=\"notice\">&lt;used&gt;</p>"));
        }
    }
}
