//! The verbs the canvas's host node (`canvas.tools`) declares (issue #642): one struct
//! per verb, then `verbs!` — the list the agent's own entry checks every call against,
//! and that `reflect` and `docs` render. `reflect` is declared once, in the kernel.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// The kernel starts the node: nothing to open. Answers nothing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// One direct child of this agent; asking for an existing child of the same handler again answers it. An AI (its handler names a memory store) is made as every AI is: on the project's memory store, granted that store and the human's chat verbs. Returns {id, …}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Create {
    /// The child: `handler_module`, `id` (optional) and its own record fields; no parent or grants of the caller's.
    #[schemars(schema_with = "child_record")]
    pub record: Map<String, Value>,
}

/// The schema of a child's record: an object naming the bundle it runs.
fn child_record(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "required": ["handler_module"],
        "properties": {
            "handler_module": {"type": "string", "minLength": 1, "description": "The bundle the child runs."}
        }
    }))
    .expect("a child record schema")
}

/// Delete an agent under this one, and its subtree.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    /// The agent to delete: one under this node.
    #[schemars(length(min = 1))]
    pub id: String,
}

/// The default AI set-up and the project's own bundles. Returns {preferred, bundles:[{name, sentence}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Presets {}

/// Make one AI cell's backend from a saved set-up (else the default; none and no name is refused, naming how to set one), or place a project bundle: `bundle` makes a NEW project bundle from that starter (only when the user asks for a bundle), `instance_of` one more instance of a project bundle. Returns {id, root, saved, handler_module, model}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    /// The saved set-up to make the AI from; absent, the owner's default.
    #[schemars(length(min = 1))]
    pub saved: Option<String>,
    /// A starter to make a NEW project bundle from.
    #[schemars(length(min = 1))]
    pub bundle: Option<String>,
    /// A project bundle to place one more instance of.
    #[schemars(length(min = 1))]
    pub instance_of: Option<String>,
    /// The caller's own fields for the backend root, copied unread; the agent's own structure (parent, id, handler) is set after it.
    pub stamp: Option<Map<String, Value>>,
    /// Any grant key: more egress for the AI.
    pub peer_grants: Option<Map<String, Value>>,
}

/// Record the dialog of a cell's AI (its rows are already in the project's memory store); it outlives the cell. Returns {kept, name}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    /// The cell's AI.
    #[schemars(length(min = 1))]
    pub id: String,
    /// The dialog's name; absent, the AI's id.
    pub name: Option<String>,
}

/// Name a kept dialog; an empty name is the AI's id. Returns {id, name}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rename {
    /// The kept dialog's AI.
    #[schemars(length(min = 1))]
    pub id: String,
    /// The new name.
    pub name: String,
}

/// The kept dialogs. Returns {dialogs:[{id, name, saved, live}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dialogs {}

/// Bind a new cell to a kept dialog: the live AI when another cell has it, else the same AI re-made here, its history intact. Returns {id, root, saved, name, shared}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reopen {
    /// The kept dialog's AI.
    #[schemars(length(min = 1))]
    pub id: String,
    /// As `spawn`'s.
    pub stamp: Option<Map<String, Value>>,
    /// As `spawn`'s.
    pub peer_grants: Option<Map<String, Value>>,
}

/// A CHILD canvas `canvas_<name>` of this one: its page records nest under this canvas's records, and deleting this canvas deletes it. Its own page kernel and its own cells' backends. Open it at the page URL with `?canvas=<id>`. Returns {id}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewCanvas {
    /// The canvas's name.
    #[schemars(length(min = 1))]
    pub name: String,
}

/// Every canvas, the main one first. Returns {canvases:[{id, loader, main, name}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Canvases {}

/// One remote MCP server as an agent under this canvas: its tools are its verbs. The key is stored in the credential store (choose where keys are kept first). Returns {id}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddMcp {
    /// The server's MCP endpoint.
    #[schemars(length(min = 1), regex(pattern = "\\S"))]
    pub url: String,
    /// A static API key, sent as Bearer to that host only.
    pub key: Option<String>,
}

/// An MCP agent's settings changed (a new key stored for the url's host and named), then it connects and lists its tools again (`boot`). Neither given: a reset, it reconnects as it is. Returns {id, url}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureMcp {
    /// An MCP agent of this canvas.
    #[schemars(length(min = 1))]
    pub id: String,
    /// A new endpoint.
    pub url: Option<String>,
    /// A new key.
    pub key: Option<String>,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "create" => Create,
    "delete" => Delete,
    "presets" => Presets,
    "spawn" => Spawn,
    "keep" => Keep,
    "rename" => Rename,
    "dialogs" => Dialogs,
    "reopen" => Reopen,
    "new_canvas" => NewCanvas,
    "canvases" => Canvases,
    "add_mcp" => AddMcp,
    "configure_mcp" => ConfigureMcp,
}
