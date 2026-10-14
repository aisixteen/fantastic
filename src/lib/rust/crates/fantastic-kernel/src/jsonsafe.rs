//! JSON assembly that never round-trips a caller's value.
//!
//! `serde_json`'s `json!` macro turns every non-literal operand into a `Value`
//! through `serde_json::to_value(..).unwrap()`, and `to_value` re-PARSES an
//! arbitrary-precision number from its stored text. This kernel stores exactly
//! one number whose text is not JSON: the `Infinity` / `-Infinity` / `NaN`
//! token python's `json.dumps(..., allow_nan=True)` writes
//! ([`crate::scalar`]). Feeding one back through `to_value` returns
//! `Error("invalid number")` — so `json!` PANICS on it and
//! `to_value(..).unwrap_or(Value::Null)` silently answers `null`.
//!
//! Either way a caller's value is lost: `fantastic_kernel kernel create_agent …
//! nan=nan` would panic in the traffic event, or print `null` where python
//! prints the record.
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

/// How a raw reply BODY is written where python's reply dict holds the bytes
/// natively (its slot — the `bytes` key for a bundle's reply).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyAs {
    /// `{len: N}` — what a memory store holds (python `_store_safe`; base64 is
    /// banned, a length is the honest record).
    Len,
    /// `"<bytes:N>"` — what a JSON wire carries (python `_json_safe`).
    Marker,
}

/// `value` with the raw `body` written at its slot, per `how`. No body ⇒ the
/// value verbatim; a body whose slot `value` cannot hold (a non-object reply) is
/// not written. A rust `Value` never holds bytes itself — the body rides beside
/// it — so this is the whole of python's `_store_safe`/`_json_safe`.
#[must_use]
pub fn with_body(value: &Value, body: Option<&crate::outcome::Blob>, how: BodyAs) -> Value {
    let Some(body) = body else {
        return value.clone();
    };
    let mut out = value.clone();
    let v = match how {
        BodyAs::Len => {
            let mut len = Map::new();
            len.insert("len".to_string(), Value::from(body.bytes.len()));
            Value::Object(len)
        }
        BodyAs::Marker => Value::String(format!("<bytes:{}>", body.bytes.len())),
    };
    if body.place(&mut out, v).is_err() {
        return value.clone();
    }
    out
}

#[cfg(test)]
#[path = "jsonsafe/tests.rs"]
mod tests;
