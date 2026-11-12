//! The fields a scheduler's record carries, beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// A scheduler's own record field.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// `true` holds every schedule: the tick loop fires nothing until it is `false` again.
    pub paused: Option<bool>,
}

fantastic_kernel::record!(Record);
