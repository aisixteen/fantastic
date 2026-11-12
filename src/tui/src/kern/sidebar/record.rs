//! The sidebar's record: no field of its own beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// The sidebar's own record fields: none.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {}

fantastic_kernel::record!(Record);
