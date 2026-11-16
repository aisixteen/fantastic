//! The fields a durable memory store agent's record carries, beside the ones every
//! record has (issue #642).

use schemars::JsonSchema;
use serde::Deserialize;

/// A SQLite store's own record fields.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// The database file: relative to the work folder (or `@state/…`, `@self/…`), never absolute and never climbing out with `..`.
    #[schemars(length(min = 1), regex(pattern = r"^(?:[^./\s][^/]*|\.|\.[^./][^/]*|\.\.[^/]+)(?:/+(?:[^./][^/]*|\.|\.[^./][^/]*|\.\.[^/]+))*/*$"))]
    pub path: String,
    /// The SQLite files the `query` verb may read, each with the same rules as `path` (relative to the work folder or `@state/…`, `@self/…`; never absolute, never climbing out with `..`). Absent or empty: none.
    #[schemars(inner(length(min = 1), regex(pattern = r"^(?:[^./\s][^/]*|\.|\.[^./][^/]*|\.\.[^/]+)(?:/+(?:[^./][^/]*|\.|\.[^./][^/]*|\.\.[^/]+))*/*$")))]
    pub query_allow: Option<Vec<String>>,
}

fantastic_kernel::record!(Record);
