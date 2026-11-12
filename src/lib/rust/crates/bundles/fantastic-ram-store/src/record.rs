//! The fields a memory store agent's record carries, beside the ones every record
//! has (issue #642). It carries none of its own: the tables live in RAM.

use schemars::JsonSchema;
use serde::Deserialize;

/// A RAM store's own record fields: none.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {}

fantastic_kernel::record!(Record);
