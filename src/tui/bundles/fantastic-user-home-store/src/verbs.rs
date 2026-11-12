//! The verbs the saved set-up store declares (issue #642): one struct per verb, then
//! `verbs!` — the list the agent's own entry checks every call against, and that
//! `reflect` and `docs` render. `reflect` is declared once, in the kernel.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// The kernel starts the store: nothing to open. Answers nothing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Save a set-up under a name: the target bundle's declared record must accept it (every field known and of its type, the required ones present). A set-up carries no identity, wiring or grants. Returns {name, version, path}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersistRecord {
    /// The set-up's name.
    #[schemars(length(min = 1), regex(pattern = "^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$"))]
    pub name: String,
    /// The set-up: `handler_module` plus that bundle's own record fields.
    #[schemars(schema_with = "set_up")]
    pub record: SetUp,
    /// The version you read; the save is refused when the file changed since.
    pub if_match: Option<String>,
}

/// A set-up as the declared schema decodes it: the bundle it is for, typed, and that
/// bundle's own fields beside it.
#[derive(Debug, Deserialize)]
pub struct SetUp {
    /// The bundle the set-up is for.
    pub handler_module: String,
    /// That bundle's own fields.
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// The schema of a saved set-up: an object naming the bundle it is for, and never
/// carrying an identity, wiring or authority field (each such field is declared with
/// no allowed value, so the shared engine refuses it).
fn set_up(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut props = serde_json::Map::new();
    props.insert(
        "handler_module".into(),
        serde_json::json!({"type": "string", "minLength": 1, "description": "The bundle the set-up is for."}),
    );
    for k in crate::NOT_SAVED {
        props.insert(
            k.into(),
            serde_json::json!({"enum": [], "description": "Never saved: the creator adds identity, wiring and authority."}),
        );
    }
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "required": ["handler_module"],
        "properties": props,
    }))
    .expect("a set-up schema")
}

/// Delete a saved set-up. Returns {name, forgotten}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgetRecord {
    /// The set-up's name.
    #[schemars(length(min = 1), regex(pattern = "^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$"))]
    pub name: String,
    /// The version you read; the delete is refused when the file changed since.
    pub if_match: Option<String>,
}

/// Every saved set-up. Returns {saved:[{name, version, record}], preferred, dir}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadTree {}

/// The owner's default set-up for new AI agents; no name clears it. Returns {preferred}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prefer {
    /// The saved set-up to prefer; absent, none.
    #[schemars(length(min = 1), regex(pattern = "^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$"))]
    pub name: Option<String>,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "persist_record" => PersistRecord,
    "forget_record" => ForgetRecord,
    "load_tree" => LoadTree,
    "prefer" => Prefer,
}
