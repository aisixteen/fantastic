//! The verbs a durable memory store declares (issue #642): the SAME verbs as
//! `ram_store` — a caller cannot tell the two apart — declared once, in
//! `fantastic-ram-store`, and used here as they are; only `boot` is this store's own,
//! since it opens the database file.

use schemars::JsonSchema;
use serde::Deserialize;

pub use fantastic_ram_store::verbs::{
    Append, Dump, GetBytes, Inject, Order, Prune, PutBytes, Read, Search, StoreTables,
};

/// Open the store's database file (the schema ensured): a bad `path` fails here, at boot, not on the first turn. Sent by the kernel at start.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Run one read-only SQL statement over one of the SQLite files in the record's `query_allow`. The file is opened read-only and a statement that would write is refused. Returns {columns, rows, truncated}; a blob is {bytes: n}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Query {
    /// One entry of `query_allow`, as written there.
    #[schemars(length(min = 1))]
    pub db: String,
    /// One statement only; `?` placeholders take `params`.
    #[schemars(length(min = 1))]
    pub sql: String,
    /// Positional values bound to the placeholders (null, boolean, number or string).
    pub params: Option<Vec<serde_json::Value>>,
    /// The most rows returned (default 500, never above 5000); `truncated` says there were more.
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "append" => Append,
    "read" => Read,
    "search" => Search,
    "prune" => Prune,
    "inject" => Inject,
    "dump" => Dump,
    "put_bytes" => PutBytes,
    "get_bytes" => GetBytes,
    "query" => Query,
}
