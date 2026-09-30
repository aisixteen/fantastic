//! THE RECORD SCHEMA of an AI backend kind — what its record takes, readable
//! with NO agent existing (owner, 2026-09-30, lib request #3). Python
//! `ai_core/schema.py`, field for field and text for text.
//!
//! A set-up page asks the catalog (`reflect bundles=all`: each entry carries the
//! bundle's `schema` when its [`Bundle::record_schema`] publishes one) and builds
//! its inputs from it; it guesses nothing. Per field: `name`, `type`
//! (`str|int|float|bool|enum|list|map`), `required`, `enum` (the legal values, for
//! an `enum`), `default` (THE BACKEND'S OWN value, present only where the backend
//! has one — a required field never has one: nothing is guessed), and a one-line
//! `hint`. `key_var` names the environment variable the backend reads its key
//! from (`null`: it takes none). Keys never ride the record.
//!
//! ONE TRUTH. The same schema is what the backend's `validate_meta` enforces AT
//! THE WRITE ([`validate`]): every required field is refused at `create_agent`/
//! `update_agent` when absent, and an `enum` field outside its values is refused
//! — so the page and the kernel cannot disagree about what a record needs. A
//! field's other checks (a positive int, a budget below `max_tokens`) stay in the
//! backend's own `validate_meta`, beside this one.
//!
//! A backend's schema is its own fields followed by [`ai_fields`] — the ones every
//! ai_core agent takes.
//!
//! [`Bundle::record_schema`]: fantastic_kernel::bundle::Bundle::record_schema

use crate::constants::{DEFAULT_CONTEXT_WINDOW, DEFAULT_OUTPUT_RESERVE, RECENT_N, RECENT_N_RANGE};
use crate::strategies::{DEFAULT_STRATEGY, STRATEGY_NAMES};
use fantastic_bundle::{py_repr_value, py_strip};
use serde_json::{json, Map, Value};

/// The field types a schema may name. Python `TYPES`.
pub const TYPES: [&str; 7] = ["str", "int", "float", "bool", "enum", "list", "map"];

/// One field, as built by a backend. [`Field::json`] renders it — and refuses
/// (panics, python's `ValueError` at import) a field python would refuse to
/// build, so a malformed schema cannot ship.
#[derive(Debug, Clone)]
pub struct Field {
    name: &'static str,
    type_: &'static str,
    hint: String,
    required: bool,
    default: Option<Value>,
    enum_: Option<Vec<String>>,
}

impl Field {
    /// An optional field with no default. Python `field(name, type_, hint)`.
    #[must_use]
    pub fn new(name: &'static str, type_: &'static str, hint: impl Into<String>) -> Self {
        Self {
            name,
            type_,
            hint: hint.into(),
            required: false,
            default: None,
            enum_: None,
        }
    }

    /// The record must name it (`required=True`).
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// The backend's own value when the record names none (`default=…`).
    #[must_use]
    pub fn default(mut self, v: Value) -> Self {
        self.default = Some(v);
        self
    }

    /// The legal values of an `enum` field (`enum=…`).
    #[must_use]
    pub fn one_of(mut self, values: &[&str]) -> Self {
        self.enum_ = Some(values.iter().map(|s| (*s).to_string()).collect());
        self
    }

    /// The field as the catalog publishes it — python's key order: `name`,
    /// `type`, `required`, `enum?`, `default?`, `hint`.
    ///
    /// # Panics
    /// On a field python's `field()` refuses: an unknown type, `enum` values
    /// without type `enum` (or the reverse), or a required field with a default.
    #[must_use]
    pub fn json(&self) -> Value {
        let name = self.name;
        assert!(
            TYPES.contains(&self.type_),
            "field {name:?}: type {:?} is not one of {TYPES:?}",
            self.type_
        );
        assert!(
            (self.type_ == "enum") == self.enum_.is_some(),
            "field {name:?}: `enum` values go with type 'enum', only"
        );
        assert!(
            !(self.required && self.default.is_some()),
            "field {name:?}: a required field has no default"
        );
        let mut out = Map::new();
        out.insert("name".into(), json!(name));
        out.insert("type".into(), json!(self.type_));
        out.insert("required".into(), json!(self.required));
        if let Some(values) = &self.enum_ {
            out.insert("enum".into(), json!(values));
        }
        if let Some(d) = &self.default {
            out.insert("default".into(), d.clone());
        }
        out.insert("hint".into(), json!(self.hint));
        Value::Object(out)
    }
}

/// The fields EVERY ai_core agent takes, whatever its backend. Python
/// `AI_FIELDS`.
#[must_use]
pub fn ai_fields() -> Vec<Field> {
    let (lo, hi) = RECENT_N_RANGE;
    vec![
        Field::new(
            "memory_store_id",
            "str",
            "the memory store agent that keeps this agent's conversation (e.g. a ram_store)",
        )
        .required(),
        Field::new("context_window", "int", "tokens the model sees per turn")
            .default(json!(DEFAULT_CONTEXT_WINDOW)),
        Field::new(
            "output_reserve",
            "int",
            "tokens of the window held back for the model's reply",
        )
        .default(json!(DEFAULT_OUTPUT_RESERVE)),
        Field::new(
            "context_strategy",
            "enum",
            "how the conversation is fitted into the window",
        )
        .one_of(&STRATEGY_NAMES)
        .default(json!(DEFAULT_STRATEGY)),
        Field::new(
            "recent_n",
            "int",
            format!("recent turns a compaction keeps verbatim ({lo}–{hi})"),
        )
        .default(json!(RECENT_N)),
    ]
}

/// A backend's whole record schema: its own fields, then [`ai_fields`] — python
/// `schema(backend_fields, key_var=…)`.
///
/// # Panics
/// On a duplicate field name, or a field [`Field::json`] refuses.
#[must_use]
pub fn schema(backend_fields: Vec<Field>, key_var: Option<&str>) -> Value {
    let fields: Vec<Field> = backend_fields.into_iter().chain(ai_fields()).collect();
    let names: Vec<&str> = fields.iter().map(|f| f.name).collect();
    let mut seen = std::collections::HashSet::new();
    assert!(
        names.iter().all(|n| seen.insert(*n)),
        "duplicate field in schema: {names:?}"
    );
    json!({
        "fields": fields.iter().map(Field::json).collect::<Vec<_>>(),
        "key_var": key_var,
    })
}

/// The schema's fields as JSON objects (an empty slice on a malformed schema —
/// every schema here is built by [`schema`]).
fn fields_of(sch: &Value) -> &[Value] {
    sch.get("fields")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn str_of<'a>(f: &'a Value, key: &str) -> &'a str {
    f.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Whether a record NAMES this field in a usable form — a string must be
/// non-blank (python `str.strip()`), anything else non-null. Python `_present`.
fn present(f: &Value, v: Option<&Value>) -> bool {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return false;
    };
    if str_of(f, "type") == "str" {
        return v.as_str().is_some_and(|s| !py_strip(s).is_empty());
    }
    true
}

/// THE WRITE-TIME CHECK the schema implies, on the MERGED record — python
/// `validate(sch, backend, meta, record)`: an `enum` field the write names must be
/// one of its values, and every required field must be present. Every missing
/// field is named at once, each with its hint, so one retry fixes the record.
#[must_use]
pub fn validate(
    sch: &Value,
    backend: &str,
    meta: &Map<String, Value>,
    record: &Map<String, Value>,
) -> Option<String> {
    for f in fields_of(sch) {
        let name = str_of(f, "name");
        let Some(v) = meta.get(name).filter(|v| !v.is_null()) else {
            continue;
        };
        if str_of(f, "type") != "enum" {
            continue;
        }
        let legal: Vec<&str> = f
            .get("enum")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !v.as_str().is_some_and(|s| legal.contains(&s)) {
            return Some(format!(
                "{name}={} is not one of the legal values: {}",
                py_repr_value(v),
                legal.join(", ")
            ));
        }
    }
    let mut merged = record.clone();
    for (k, v) in meta {
        merged.insert(k.clone(), v.clone());
    }
    let missing: Vec<&Value> = fields_of(sch)
        .iter()
        .filter(|f| f.get("required") == Some(&Value::Bool(true)))
        .filter(|f| !present(f, merged.get(str_of(f, "name"))))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let names: Vec<&str> = missing.iter().map(|f| str_of(f, "name")).collect();
    let hints: Vec<String> = missing
        .iter()
        .map(|f| format!("{} = {}", str_of(f, "name"), str_of(f, "hint")))
        .collect();
    Some(format!(
        "{backend}: required on the record and missing: {} — nothing is guessed. {}",
        names.join(", "),
        hints.join("; ")
    ))
}

/// The required field names of a schema, in order — a backend's `REQUIRED`
/// derived from its schema, never a second list.
#[must_use]
pub fn required_names(sch: &Value) -> Vec<String> {
    fields_of(sch)
        .iter()
        .filter(|f| f.get("required") == Some(&Value::Bool(true)))
        .map(|f| str_of(f, "name").to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn sample() -> Value {
        schema(
            vec![
                Field::new("endpoint", "str", "the URL").required(),
                Field::new("mode", "enum", "a or b").one_of(&["a", "b"]),
            ],
            Some("KEY"),
        )
    }

    #[test]
    fn a_schema_is_the_backend_fields_then_the_ai_fields() {
        let sch = sample();
        let names: Vec<&str> = fields_of(&sch).iter().map(|f| str_of(f, "name")).collect();
        assert_eq!(
            names,
            [
                "endpoint",
                "mode",
                "memory_store_id",
                "context_window",
                "output_reserve",
                "context_strategy",
                "recent_n"
            ]
        );
        assert_eq!(sch["key_var"], "KEY");
        assert_eq!(
            sch["fields"][6]["hint"],
            "recent turns a compaction keeps verbatim (1–50)"
        );
        assert_eq!(required_names(&sch), ["endpoint", "memory_store_id"]);
    }

    #[test]
    fn every_missing_field_is_named_with_its_hint() {
        let got = validate(&sample(), "x_backend", &Map::new(), &Map::new());
        assert_eq!(
            got.as_deref(),
            Some(
                "x_backend: required on the record and missing: endpoint, memory_store_id — \
                 nothing is guessed. endpoint = the URL; memory_store_id = the memory store \
                 agent that keeps this agent's conversation (e.g. a ram_store)"
            )
        );
        // a blank string is not a value; a write of null clears the record's value
        let rec = obj(json!({"endpoint": "http://h", "memory_store_id": "m"}));
        assert!(
            validate(&sample(), "x", &obj(json!({"endpoint": "  "})), &rec)
                .is_some_and(|e| e.contains("missing: endpoint —"))
        );
        assert!(
            validate(&sample(), "x", &obj(json!({"memory_store_id": null})), &rec)
                .is_some_and(|e| e.contains("missing: memory_store_id —"))
        );
        assert_eq!(validate(&sample(), "x", &Map::new(), &rec), None);
    }

    #[test]
    fn an_enum_value_outside_its_set_is_refused_in_pythons_words() {
        let rec = obj(json!({"endpoint": "http://h", "memory_store_id": "m"}));
        assert_eq!(
            validate(&sample(), "x", &obj(json!({"mode": "c"})), &rec).as_deref(),
            Some("mode='c' is not one of the legal values: a, b")
        );
        assert_eq!(
            validate(&sample(), "x", &obj(json!({"context_strategy": 3})), &rec).as_deref(),
            Some(
                "context_strategy=3 is not one of the legal values: compact, truncate, strip, fold"
            )
        );
        // null is "not named" — never judged against the set
        assert_eq!(
            validate(&sample(), "x", &obj(json!({"mode": null})), &rec),
            None
        );
    }

    #[test]
    #[should_panic(expected = "a required field has no default")]
    fn a_required_field_with_a_default_cannot_ship() {
        let _ = Field::new("x", "int", "h")
            .required()
            .default(json!(1))
            .json();
    }

    #[test]
    #[should_panic(expected = "`enum` values go with type 'enum', only")]
    fn enum_values_need_the_enum_type() {
        let _ = Field::new("x", "str", "h").one_of(&["a"]).json();
    }

    #[test]
    #[should_panic(expected = "duplicate field in schema")]
    fn a_backend_field_may_not_shadow_an_ai_field() {
        let _ = schema(vec![Field::new("recent_n", "int", "h")], None);
    }
}
