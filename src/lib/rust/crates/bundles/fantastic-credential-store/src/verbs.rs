//! The verbs a credential store declares (issue #642): one struct per verb, then
//! `verbs!` — the list the agent's entry checks every call against and `reflect`
//! and `docs` render.

use std::collections::BTreeMap;

use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use schemars::JsonSchema;
use serde::Deserialize;

/// A map of field name to non-empty text.
fn text_fields(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "additionalProperties": {"type": "string", "minLength": 1},
    }))
    .expect("a map schema")
}

/// Store or rotate the value of a DECLARED ref — its host is the declaration's (refs are declared by update_agent on this store's record; set never creates one and takes no host). Returns {ref, host, fields:[names], stored}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Set {
    /// The declared ref.
    #[serde(rename = "ref")]
    #[schemars(length(min = 1))]
    pub ref_name: String,
    /// The value: a non-empty map of field name to string value.
    #[schemars(schema_with = "text_fields")]
    pub fields: BTreeMap<String, String>,
}

/// The credential: {ref, host, fields:{name: value}}. Send the value ONLY to `host`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Get {
    /// The declared ref.
    #[serde(rename = "ref")]
    #[schemars(length(min = 1))]
    pub ref_name: String,
    /// Where you will send it — refused unless the ref is bound to it.
    pub host: Option<String>,
}

/// The declared refs — metadata only: {refs:[{ref, host, fields:[names]}]}. Never a value.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {}

/// Drop a ref's VALUE from the backend; its declaration (host) stays — update_agent alone undeclares a ref. Returns {forgotten, ref}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Forget {
    /// The declared ref.
    #[serde(rename = "ref")]
    #[schemars(length(min = 1))]
    pub ref_name: String,
}

fantastic_kernel::verbs! {
    "set" => Set,
    "get" => Get,
    "list" => List,
    "forget" => Forget,
}
