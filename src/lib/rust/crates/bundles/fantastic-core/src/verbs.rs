//! The verbs `kernel_state.tools` declares (issue #642): one struct per verb, then
//! `verbs!` — the list the loader's own entry (`Call::parse`) checks every call against and `reflect`
//! and `docs`. `reflect` is the verbs every agent answers; it is declared once, in
//! the kernel.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Start the auto-flush loop and install the record writer, once (a second boot installs nothing). Sent by the kernel at start.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Read this loader's subtree from disk → {records}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadTree {}

/// Merge-write one agent.yaml: the location is the record's place in the tree (an existing agent keeps its position; `parent_id` places only a new one).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersistRecord {
    /// The agent record: an object that carries the agent's `id` and any other fields.
    #[schemars(schema_with = "record_with_id")]
    pub record: Map<String, Value>,
}

/// The schema of a persisted record: an open object whose `id` (and `parent_id`,
/// when it has one) is text — every other field is the agent's own.
fn record_with_id(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "required": ["id"],
        "properties": {
            "id": {"type": "string", "description": "The agent's id."},
            "parent_id": {"type": ["string", "null"], "description": "Where a new agent sits in the tree."}
        }
    }))
    .expect("a record schema")
}

/// Remove one agent's folder and everything under it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgetRecord {
    /// The agent whose folder goes.
    pub id: String,
    /// Where an agent that is no longer live sat in the tree.
    pub parent_id: Option<String>,
}

/// Persist an agent's sidecar file THROUGH the loader's store: the caller names itself and a filename, never a path.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersistBlob {
    /// The agent the file belongs to (one safe path segment).
    pub agent_id: String,
    /// One bare filename.
    pub name: String,
    /// The file's text.
    pub content: String,
}

/// Read an agent's sidecar file THROUGH the loader's store.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadBlob {
    /// The agent the file belongs to.
    pub agent_id: String,
    /// One bare filename.
    pub name: String,
}

/// Re-read `bundles/<name>/bundle.yaml` (a new folder loads) and restart every live instance of it, abruptly — an in-flight call is answered with an error naming the reload. Returns {reloaded, instances}. The root answers it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReloadBundle {
    /// A folder under `.fantastic/bundles/`.
    #[schemars(length(min = 1))]
    pub name: String,
}

/// The folder bundles under this loader's root AS TEXT — {bundles:[{name, folder, text?, error?}]}, each `bundles/<name>/bundle.yaml` raw (`error` when it has none). For a kernel that loads its own folders through this loader (a page kernel's proxy_loader); this loader parses nothing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadBundles {
    /// One folder only; absent, all of them.
    #[schemars(length(min = 1))]
    pub name: Option<String>,
}

/// Write an agent and everything under it as a blueprint folder: outside references become ports; no keys, no absolute paths, no rows. Reply {exported, path, ports, files}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportSubtree {
    /// The agent whose subtree is written.
    pub agent_id: String,
    /// A new or empty folder, relative to the running dir (no `..`).
    pub path: String,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "load_tree" => LoadTree,
    "persist_record" => PersistRecord,
    "forget_record" => ForgetRecord,
    "persist_blob" => PersistBlob,
    "load_blob" => LoadBlob,
    "reload_bundle" => ReloadBundle,
    "read_bundles" => ReadBundles,
    "export_subtree" => ExportSubtree,
}
