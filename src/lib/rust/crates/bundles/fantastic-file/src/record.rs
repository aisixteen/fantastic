//! The fields a file bridge agent's record carries, beside the ones every record
//! has (issue #642). A VIEW of one path is a record of the same bundle that also
//! carries `view`, `bridge` and `path`.

use schemars::JsonSchema;
use serde::Deserialize;


fn default_readonly() -> Option<bool> {
    Some(false)
}

/// A file bridge's own record fields.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The directory the bridge serves: relative to the work folder, or `@state/…`, `@self/…`; every path is clamped inside it.
    pub root: Option<String>,
    /// `true` refuses every mutating verb.
    #[schemars(default = "default_readonly")]
    pub readonly: Option<bool>,
    /// What the bridge exposes, relative to `root`: a file, or everything under a directory (`.` is the whole root). No list, or an empty one, exposes nothing.
    #[schemars(inner(length(min = 1), regex(pattern = r"^(?:(?:[^./][^/]*|\.|\.[^./][^/]*|\.\.[^/]+)/+)*(?:[^./][^/]*|\.|\.[^./][^/]*|\.\.[^/]+)/*$")))]
    pub allow: Option<Vec<String>>,
    /// Who may reach the bridge; absent, the bridge is sealed.
    pub ingress_rule: Option<fantastic_io_bridge::authorizer::EnvelopelessIngress>,
    /// A view's bridge: the agent id of the file bridge it shows.
    pub bridge: Option<String>,
    /// A view's path, relative to its bridge's root.
    pub path: Option<String>,
}

fantastic_kernel::record!(Record);
