//! The record of the saved set-up store (issue #642): it carries no field of its own
//! beside the ones every record has — the store's directory is fixed by the binary,
//! never by a record.

use schemars::JsonSchema;
use serde::Deserialize;

/// The saved set-up store's own record fields: none.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {}

fantastic_kernel::record!(Record);
