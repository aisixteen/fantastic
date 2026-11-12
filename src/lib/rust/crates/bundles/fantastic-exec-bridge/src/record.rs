//! The fields an exec_bridge agent's record carries, beside the ones every record
//! has (issue #642). WHERE a command runs and WITH WHAT are the record's, never the
//! call's: the declaration shapes them at the write.

use std::collections::BTreeMap;

use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// A non-empty list of non-empty names.
fn names(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["array", "null"],
        "items": {"type": "string", "minLength": 1},
    }))
    .expect("a list schema")
}

/// A non-empty list of regexes.
fn patterns(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["array", "null"],
        "minItems": 1,
        "items": {"type": "string"},
    }))
    .expect("a list schema")
}

/// A map of NAME to a scalar value.
fn variables(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["object", "null"],
        "additionalProperties": {"type": ["string", "number", "boolean"]},
    }))
    .expect("a map schema")
}

/// `true` (the child's `$SHELL`, bare), `"login"` (`$SHELL -l`) or `false` (none).
fn shell(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "anyOf": [{"type": "boolean"}, {"type": "string", "enum": ["login"]}, {"type": "null"}],
    }))
    .expect("a shell schema")
}

/// An exec_bridge's own record fields.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// Who may reach the bridge: `allow_all_ingress` or `deny_ingress`; absent, the bridge is sealed.
    pub ingress_rule: Option<Value>,
    /// The both-directions shorthand of the ingress and egress rules.
    pub auth: Option<Value>,
    /// Regexes a command line (command + args, shell-joined) must fullmatch to run; none runs nothing.
    #[serde(default)]
    #[schemars(schema_with = "patterns")]
    pub allowed: Option<Vec<String>>,
    /// Where commands run: a folder under the work folder, or `@state/…`; required to start anything.
    #[schemars(length(min = 1))]
    pub cwd: Option<String>,
    /// Extra environment of the child: NAME to value.
    #[serde(default)]
    #[schemars(schema_with = "variables")]
    pub env: Option<BTreeMap<String, Value>>,
    /// The kernel's environment variables the child inherits (`["*"]` = all).
    #[serde(default)]
    #[schemars(schema_with = "names")]
    pub inherit_env: Option<Vec<String>>,
    /// The shell line permitted without an `allowed` entry: `true` (the child's `$SHELL`), `"login"` (`$SHELL -l`) or `false`.
    #[serde(default)]
    #[schemars(schema_with = "shell")]
    pub shell: Option<Value>,
}

fantastic_kernel::record!(Record);
