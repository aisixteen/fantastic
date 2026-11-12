//! The fields the canvas's host node (`canvas.tools`) record carries, beside the ones
//! every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// The canvas node's own record fields.
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct Record {
    /// `true` marks the main canvas, listed first.
    pub main: Option<bool>,
}

fantastic_kernel::record!(Record);
