//! JSON assembly that never round-trips a caller's value.
//!
//! `serde_json`'s `json!` macro turns every non-literal operand into a `Value`
//! through `serde_json::to_value(..).unwrap()`, and `to_value` re-PARSES an
//! arbitrary-precision number from its stored text. This kernel stores exactly
//! one number whose text is not JSON: the `Infinity` / `-Infinity` / `NaN`
//! token python's `json.dumps(..., allow_nan=True)` writes (C74,
//! [`crate::scalar`]). Feeding one back through `to_value` returns
//! `Error("invalid number")` — so `json!` PANICS on it and
//! `to_value(..).unwrap_or(Value::Null)` silently answers `null`.
//!
//! Both were measured: `fantastic_kernel kernel create_agent … nan=nan`
//! exited 101 with a panic in the traffic event, and with that fixed it
//! printed `null` where python prints the record.
//!
//! So nothing that carries a value a CALLER supplied — a payload, a reply, an
//! agent record, an outcome — may be assembled with `json!`. Use [`obj`], which
//! moves the values in as they are.

use crate::agent::AgentRecord;
use serde_json::{Map, Value};

/// A JSON object from `(key, value)` pairs, values moved in verbatim.
///
/// The one safe way to put a caller's `Value` inside another object.
#[must_use]
pub(crate) fn obj<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut out = Map::with_capacity(N);
    for (k, v) in pairs {
        out.insert(k.to_string(), v);
    }
    Value::Object(out)
}

/// One agent record as JSON, in python's key order: `id`, `handler_module`,
/// the meta in insertion order, then `parent_id` (structure wins — see
/// [`AgentRecord`]). Optional fields are omitted when absent, exactly as the
/// derived `Serialize` omits them.
///
/// Hand-assembled for the reason this module exists: `serde_json::to_value`
/// over the derived impl walks the flattened meta and dies on a caller's
/// `NaN`, so the record a `create_agent` answers with became `null`.
#[must_use]
pub(crate) fn record_value(rec: &AgentRecord) -> Value {
    let mut out = Map::with_capacity(rec.meta.len() + 3);
    out.insert("id".to_string(), Value::String(rec.id.clone()));
    if let Some(hm) = &rec.handler_module {
        out.insert("handler_module".to_string(), Value::String(hm.clone()));
    }
    for (k, v) in &rec.meta {
        out.insert(k.clone(), v.clone());
    }
    if let Some(pid) = &rec.parent_id {
        out.insert("parent_id".to_string(), Value::String(pid.clone()));
    }
    Value::Object(out)
}

#[cfg(test)]
#[path = "jsonsafe/tests.rs"]
mod tests;
