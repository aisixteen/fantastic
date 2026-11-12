//! The set-up agent's record: no field of its own beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// The set-up agent's own record fields: none.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {}

fantastic_kernel::record!(Record);
