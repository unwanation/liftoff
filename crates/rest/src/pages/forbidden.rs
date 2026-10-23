//! The page of a refusal (`403`).

use super::{document, escape};

/// The refusal `reason` (safe to show; escaped) as a page.
#[must_use]
pub fn forbidden(reason: &str) -> String {
    document(
        "Forbidden",
        &format!(
            "<h1>No access</h1>\n<p>{}</p>\n<p>Sign in as someone who may open this address, \
             or ask its owner to share it (<code>lof env share</code>).</p>\n",
            escape(reason)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    mod forbidden {
        use super::*;

        // TC: OTL-08 (the reason is shown, escaped)
        #[test]
        fn shows_escaped_reason_when_rendered() {
            let page = forbidden("<b>bob</b> may not open api.apps.test");
            assert!(page.contains("&lt;b&gt;bob&lt;/b&gt; may not open api.apps.test"));
        }
    }
}
