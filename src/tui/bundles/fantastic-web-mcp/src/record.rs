//! The fields an MCP leg's record carries, beside the ones every record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

use fantastic_kernel::inquiry::Held;

/// An MCP leg's own record fields.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The file bridge the leg reads and writes the client's MCP config through; absent, the config is left alone.
    pub file_bridge_id: Option<String>,
    /// The config's path on that bridge (e.g. `.mcp.json`); required when `file_bridge_id` is set.
    pub mcp_config_file: Option<String>,
    /// The questions held for the person, open until answered (written by the leg).
    pub open_inquiries: Option<Vec<Held>>,
}

fantastic_kernel::record!(Record);
