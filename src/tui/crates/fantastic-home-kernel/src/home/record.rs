//! The fields the `home` agent's record carries, beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// The home's own record fields: what it keeps across restarts.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The ids of the open pages, in order.
    pub pages: Option<Vec<String>>,
    /// The id of the page shown; absent, the Home.
    pub selection: Option<String>,
}

fantastic_kernel::record!(Record);
