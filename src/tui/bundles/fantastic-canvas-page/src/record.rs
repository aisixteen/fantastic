//! The fields a canvas page's record carries, beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// A canvas page's own record field.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The source verbs the page declares to its `web` host (`read_stream`, as [`crate::read_stream_declared`] renders it); the host serves an agent that declares `read_stream`.
    pub verbs: Option<Value>,
}

fantastic_kernel::record!(Record);
