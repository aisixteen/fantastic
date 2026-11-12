//! The verbs a memory store declares (issue #642): one struct per verb, then
//! `verbs!` — the list the agent's entry checks every call against and `reflect`
//! and `docs` render. `sqlite_store` answers the same verbs and uses these
//! declarations as they are.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Which end of the id order `read` starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// Oldest first.
    Asc,
    /// Newest first.
    Desc,
}

/// Append rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Append {
    /// The rows to add: each an object.
    #[schemars(length(min = 1))]
    pub rows: Vec<Map<String, Value>>,
}

/// Read the caller's rows whole, oldest first by default. Returns {rows, next, total}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Read {
    /// Only rows after this id.
    pub after: Option<i64>,
    /// Only rows before this id.
    pub before: Option<i64>,
    /// Page size; absent or 0 reads every row.
    pub limit: Option<u64>,
    /// `desc` reads newest first.
    pub order: Option<Order>,
    /// Only rows whose `kind` is one of these.
    pub kinds: Option<Vec<Value>>,
    /// Only rows whose fields equal these (exact equality).
    pub r#match: Option<Map<String, Value>>,
    /// Only these row ids.
    pub ids: Option<Vec<i64>>,
    /// Start at the newest row of this kind.
    pub from_last_kind: Option<String>,
}

/// Find every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Search {
    /// The text to find, case-insensitive substring.
    #[schemars(length(min = 1), regex(pattern = "\\S"))]
    pub query: String,
    /// The rows to search; a fold id includes its members. Absent, every row.
    pub ids: Option<Vec<i64>>,
    /// How many matches to return; 100 when absent or 0.
    pub limit: Option<u64>,
    /// Characters kept per matching line; 200 when absent or 0.
    pub width: Option<u64>,
}

/// Delete the caller's rows below `before_id`, or exactly `ids`, except kinds in `keep`; every fold ref naming a deleted row goes with it (FK cascade). With neither `before_id` nor `ids`, nothing is deleted. Returns {pruned}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prune {
    /// Delete rows with an id below this.
    pub before_id: Option<i64>,
    /// Delete exactly these rows.
    pub ids: Option<Vec<i64>>,
    /// Kinds that are kept.
    pub keep: Option<Vec<Value>>,
}

/// The tables of a store: what `dump` answers and `inject` takes.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreTables {
    /// The rows: [{owner, row}].
    pub rows: Option<Vec<Value>>,
    /// The folds: [{owner, fold, members}].
    pub fold_refs: Option<Vec<Value>>,
}

/// Replace the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inject {
    /// A `dump`: {rows:[{owner, row}], fold_refs:[{owner, fold, members}]}.
    pub tables: StoreTables,
}

/// The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}]} — exactly what `inject` takes back. Returns {tables}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dump {}

/// Store raw bytes under the CALLER's key and `ref`; the bytes are the request body at `data` (a second put to the same ref replaces them). They are not rows: `read`, `search` and `dump` never show them. Returns {ref, media_type, bytes} (the byte count).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PutBytes {
    /// The name the bytes are kept under.
    #[serde(rename = "ref")]
    #[schemars(length(min = 1))]
    pub ref_name: String,
    /// What the bytes are.
    #[schemars(length(min = 1))]
    pub media_type: String,
}

/// The bytes the caller put under `ref`, as the reply body at `data`. Returns {ref, media_type, bytes} (the byte count), or not_found.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetBytes {
    /// The name the bytes were kept under.
    #[serde(rename = "ref")]
    #[schemars(length(min = 1))]
    pub ref_name: String,
}

fantastic_kernel::verbs! {
    "append" => Append,
    "read" => Read,
    "search" => Search,
    "prune" => Prune,
    "inject" => Inject,
    "dump" => Dump,
    "put_bytes" => PutBytes,
    "get_bytes" => GetBytes,
}
