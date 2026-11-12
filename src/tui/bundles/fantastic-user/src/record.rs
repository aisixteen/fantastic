//! The fields `user`'s record carries, beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

use fantastic_kernel::inquiry::Held;

/// `user`'s own record fields.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The page loader (the main canvas's `kernel_state` agent) `around` reads the page records from.
    #[schemars(length(min = 1))]
    pub page_loader: Option<String>,
    /// The questions held for the person, open until answered (written by `user`).
    pub open_inquiries: Option<Vec<Held>>,
}

fantastic_kernel::record!(Record);
