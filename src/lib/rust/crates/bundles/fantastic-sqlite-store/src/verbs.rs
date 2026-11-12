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
}
