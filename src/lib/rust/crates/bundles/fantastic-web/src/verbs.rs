//! The verbs `web` declares (issue #642): one struct per verb, then `verbs!`.

use schemars::JsonSchema;
use serde::Deserialize;

/// Build the host on the record's `port` (required), mount every child's surface, then start listening. Idempotent. Returns {running, surfaces}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Stop listening and cancel the serve task. Returns {stopped}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stop {}

/// Pull `get_routes` from the named child and (re)mount its routes onto the live host; any prior routes of the child go first. Returns {mounted}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// The child agent whose surface is mounted.
    #[schemars(length(min = 1))]
    pub child_id: String,
}

/// Remove every route that belongs to the named child. Returns {unmounted}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unmount {
    /// The child agent whose routes go.
    #[schemars(length(min = 1))]
    pub child_id: String,
}

/// One `read_stream` chunk of a file on `agent_id`, read AS THIS HOST: the file route hands every browser fetch here, so the served agent's gates judge `web`. Returns that agent's reply.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchFile {
    /// The agent that serves the file.
    #[schemars(length(min = 1))]
    pub agent_id: String,
    /// The file's path on that agent.
    pub path: String,
    /// Where the chunk starts.
    pub offset: i64,
    /// How many bytes the chunk holds at most.
    pub length: i64,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "stop" => Stop,
    "mount" => Mount,
    "unmount" => Unmount,
    "fetch_file" => FetchFile,
}
