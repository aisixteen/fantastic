//! The fields `user`'s record carries, beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// `user`'s own record field.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The page loader (the main canvas's `kernel_state` agent) `around` reads the page records from.
    #[schemars(length(min = 1))]
    pub page_loader: Option<String>,
}

fantastic_kernel::record!(Record);
