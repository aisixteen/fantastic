//! The fields a credential_store record carries, beside the ones every record has
//! (issue #642). The declared refs are a map of declared objects: each ref's host
//! is required, its field names and its variable names are non-empty text.

use std::collections::BTreeMap;

use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use schemars::JsonSchema;
use serde::Deserialize;

/// Which store backs the refs: `file` (a YAML file at an absolute path), `keychain` (the OS
/// keychain) or `env` (environment variables, read-only). The variants carry no docs of their
/// own: documented variants render as `const` alternatives the engine does not check, plain
/// ones as an `enum` it does.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[allow(missing_docs)]
pub enum BackendKind {
    #[default]
    File,
    Keychain,
    Env,
}

/// The one field of a stored record that picks the backend.
#[derive(Debug, Default, Deserialize)]
pub struct Kind {
    /// The record's `backend`; absent, the default.
    pub backend: Option<BackendKind>,
}

/// A list of non-empty names.
fn names(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["array", "null"],
        "items": {"type": "string", "minLength": 1},
    }))
    .expect("a list schema")
}

/// A map of field name to variable name, both non-empty.
fn variables(_gen: &mut SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
        "type": ["object", "null"],
        "additionalProperties": {"type": "string", "minLength": 1},
    }))
    .expect("a map schema")
}

/// One declared ref: the one host its value may be sent to, and the names of what it holds.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ref {
    /// The host (or a URL of it) the ref's value may be sent to — the binding.
    #[schemars(regex(pattern = r"^\s*(?:[A-Za-z][A-Za-z0-9+.\-]*://)?[^\s/?#:]\S*\s*$"))]
    pub host: String,
    /// The names of the stored value's fields.
    #[serde(default)]
    #[schemars(schema_with = "names")]
    pub fields: Option<Vec<String>>,
    /// For the `env` backend: each field's environment variable.
    #[serde(default)]
    #[schemars(schema_with = "variables")]
    pub env: Option<BTreeMap<String, String>>,
}

/// The fields a credential_store record carries.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Record {
    /// Which store backs the refs; `file` by default.
    pub backend: Option<BackendKind>,
    /// The file of the `file` backend: an absolute path (`/…`, a drive or UNC path) or `~…`; absent, `<home>/user/credentials/<store id>.yaml`.
    #[schemars(regex(pattern = r"^(?:/|~|[A-Za-z]:[\\/]|\\\\)"))]
    pub file: Option<String>,
    /// The keychain namespace of the `keychain` backend; `fantastic` by default.
    #[schemars(length(min = 1))]
    pub keychain_namespace: Option<String>,
    /// The declared refs by name: each `{host, fields?, env?}` — the one host the ref's value may be sent to.
    pub refs: Option<BTreeMap<String, Ref>>,
}

fantastic_kernel::record!(Record);
