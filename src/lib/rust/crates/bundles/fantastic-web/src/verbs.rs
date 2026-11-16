//! The verbs `web` declares (issue #642): one struct per verb, then `verbs!`.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;

/// Build the host on the record's `port` (required), mount every child's surface, then start listening. Idempotent. Returns {running, surfaces}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Release the listener before the kernel stops; sent by the kernel at shutdown.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {}

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

/// One outbound HTTP call to a host in the record's `request_allow` (scheme, host and port must match; a redirect is followed only inside the list). A credential-store ref bound to the host supplies the Authorization header; the caller never passes one. A method other than GET/HEAD is a write (the kernel's call row records it). The reply body is capped at 1048576 bytes. Returns {status, headers, json | text | bytes (base64), truncated}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// GET, HEAD, POST, PUT, PATCH, DELETE or OPTIONS.
    #[schemars(length(min = 1))]
    pub method: String,
    /// The absolute http(s) URL; refused unless its scheme, host and port are in `request_allow`.
    #[schemars(length(min = 1))]
    pub url: String,
    /// Request headers (authorization, proxy-authorization, cookie and host are refused).
    pub headers: Option<BTreeMap<String, String>>,
    /// Query parameters appended to the URL (text, number or boolean values).
    pub query: Option<serde_json::Map<String, serde_json::Value>>,
    /// A JSON request body (sent as application/json); not with `body`.
    pub json: Option<serde_json::Value>,
    /// A text request body; not with `json`.
    pub body: Option<String>,
    /// Seconds the whole call (redirects included) may take; 30 when absent, at most 120.
    #[schemars(range(min = 0.001, max = 120.0))]
    pub timeout: Option<f64>,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "shutdown" => Shutdown,
    "stop" => Stop,
    "mount" => Mount,
    "unmount" => Unmount,
    "fetch_file" => FetchFile,
    "request" => Request,
}
