//! The page of a tunnel without its `lof expose`.

use super::document;

/// `liftoff: dev offline` as a page.
#[must_use]
pub fn offline() -> String {
    document(
        "dev offline",
        "<h1>liftoff: dev offline</h1>\n<p>The <code>lof expose</code> behind this address is not \
         connected. The address answers again once <code>lof expose</code> runs.</p>\n",
    )
}
