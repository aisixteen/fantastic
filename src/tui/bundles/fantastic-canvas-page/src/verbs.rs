//! The verbs a canvas page declares (issue #642): one struct per verb, then `verbs!` —
//! the list the agent's own entry checks every call against, and that `reflect` and
//! `docs` render. `reflect` is declared once, in the kernel.

use schemars::JsonSchema;
use serde::Deserialize;

/// A file of the embedded canvas page as a BINARY reply: the bytes as the body, {path, offset, next_offset, eof, size} as the head.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadStream {
    /// The served file's path, as the page names it (no leading slash).
    #[schemars(length(min = 1), regex(pattern = "^[^/]"))]
    pub path: String,
    /// Where the chunk starts, in bytes.
    #[serde(default)]
    pub offset: u64,
    /// The chunk size in bytes; absent, the rest of the file.
    pub length: Option<u64>,
}

fantastic_kernel::verbs! {
    "read_stream" => ReadStream,
}
