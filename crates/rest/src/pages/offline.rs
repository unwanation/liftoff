//! The page of a tunnel without its `lof dev`.

use super::document;

/// `liftoff: dev offline` as a page.
#[must_use]
pub fn offline() -> String {
    document(
        "dev offline",
        "<h1>liftoff: dev offline</h1>\n<p>The <code>lof dev</code> behind this address is not \
         connected. The address answers again once <code>lof dev</code> runs.</p>\n",
    )
}
