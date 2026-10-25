//! THE RECORD SCHEMA of an AI backend kind — what its record takes, readable
//! with NO agent existing. Python
//! `ai_core/schema.py`, field for field and text for text.
//!
//! A set-up page asks the catalog (`reflect bundles=all`: each entry carries the
//! bundle's `schema` when its [`Bundle::record_schema`] publishes one) and builds
//! its inputs from it; it guesses nothing. Per field: `name`, `type`
//! (`str|int|float|bool|enum|list|map|credentials`), `required`, `enum` (the
//! legal values, for an `enum`), `default` (THE BACKEND'S OWN value, present only
//! where the backend has one — a required field never has one: nothing is
//! guessed), and a one-line `hint`. Keys never ride the record: a backend that
//! sends one has a `credentials` field (type `credentials`, `{store, ref}` — the
//! credential store and the ref it reads the key from at each request,
//! [`crate::credentials`]).
//!
//! ONE TRUTH. The same schema is what the backend's `validate_meta` enforces AT
//! THE WRITE ([`validate`]): every required field is refused at `create_agent`/
//! `update_agent` when absent, and an `enum` field outside its values is refused
//! — and a `credentials` value must be `{store, ref}` — so the page and the
//! kernel cannot disagree about what a record needs. A
//! field's other checks (a positive int, a budget below `max_tokens`) stay in the
//! backend's own `validate_meta`, beside this one.
//!
//! A backend's schema is its own fields followed by [`ai_fields`] — the ones every
//! ai_core agent takes. One of them, `system_prompt`, is checked here too: named on
//! a write it must be a non-empty string (`null` clears it — the default block
//! again).
//!
//! [`Bundle::record_schema`]: fantastic_kernel::bundle::Bundle::record_schema

use crate::constants::{
    DEFAULT_CONTEXT_WINDOW, DEFAULT_OUTPUT_RESERVE, MAX_SILENT_TURNS, RECENT_N, RECENT_N_RANGE,
};
use crate::credentials::shape_error;
use crate::strategies::{DEFAULT_STRATEGY, STRATEGY_NAMES};
use fantastic_bundle::{py_repr_value, py_strip};
use serde_json::{json, Map, Value};

/// The field types a schema may name. Python `TYPES`.
pub const TYPES: [&str; 8] = [
    "str",
    "int",
    "float",
    "bool",
    "enum",
    "list",
    "map",
    "credentials",
];

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
    ai_fields_with_window(DEFAULT_CONTEXT_WINDOW)
}

/// [`ai_fields`] with `context_window` defaulting to `context_window` — the
/// backend's own window ([`crate::agent_loop::BackendConfig::model_window`]),
/// where it can read one.
#[must_use]
pub fn ai_fields_with_window(context_window: i64) -> Vec<Field> {
    let (lo, hi) = RECENT_N_RANGE;
    vec![
        Field::new(
            "memory_store_id",
            "str",
            "the memory store agent that keeps this agent's conversation (e.g. a ram_store)",
        )
        .required(),
        Field::new("context_window", "int", "tokens the model sees per turn")
            .default(json!(context_window)),
        Field::new(
            "output_reserve",
            "int",
            "tokens of the window held back for the model's reply",
        )
        .default(json!(DEFAULT_OUTPUT_RESERVE)),
        Field::new(
            "context_strategy",
            "enum",
            "how the conversation is fitted into the window (none = never shrink: an over-budget turn is refused)",
        )
        .one_of(&STRATEGY_NAMES)
        .default(json!(DEFAULT_STRATEGY)),
        Field::new(
            "recent_n",
            "int",
            format!("recent turns a compaction keeps verbatim ({lo}–{hi})"),
        )
        .default(json!(RECENT_N)),
        Field::new("caching", "enum", crate::caching::CACHING_HINT)
            .one_of(&crate::caching::MODES)
            .default(json!(crate::caching::DEFAULT_MODE)),
        Field::new(
            "cache_primer_until",
            "int",
            crate::caching::CACHE_PRIMER_HINT,
        ),
        Field::new("transport_retries", "int", TRANSPORT_RETRIES_HINT).default(json!(0)),
        Field::new("misfire_limit", "int", MISFIRE_LIMIT_HINT).default(json!(MAX_SILENT_TURNS)),
        Field::new("system_prompt", "str", SYSTEM_PROMPT_HINT),
        Field::new("file_bridge_id", "str", FILE_BRIDGE_ID_HINT),
        Field::new("system_prompt_file", "str", SYSTEM_PROMPT_FILE_HINT),
        Field::new("pre_system_prompt", "str", PRE_SYSTEM_PROMPT_HINT),
        Field::new("post_system_prompt", "str", POST_SYSTEM_PROMPT_HINT),
    ]
}

/// The `transport_retries` field's hint (#198 part 2).
pub const TRANSPORT_RETRIES_HINT: &str = "how many times a turn whose link dropped (connect failed, the stream cut or stalled, HTTP 502/503/504) is retried before it fails, with a 2 s, 4 s, 8 s… backoff; each retry is a visible row; 0–5, 0 = off";

/// The `misfire_limit` field's hint (#203).
pub const MISFIRE_LIMIT_HINT: &str = "answers in a row that ran nothing (wrapped, malformed, thinking-only, no call) before the chain gives up; from the 2nd one on, the reply escalates (2–50, default 10) (#203)";

/// The `pre_system_prompt` field's hint. Python `AI_FIELDS`.
pub const PRE_SYSTEM_PROMPT_HINT: &str = "text placed BEFORE the system block on every turn \
     — whichever is in force (the default, `system_prompt` or `system_prompt_file`), never \
     replacing it; verbatim, no placeholders; null = nothing";

/// The `post_system_prompt` field's hint. Python `AI_FIELDS`.
pub const POST_SYSTEM_PROMPT_HINT: &str = "text placed AFTER the system block on every turn \
     — whichever is in force, never replacing it; verbatim, no placeholders; null = nothing";

/// The refusal for a `pre_system_prompt` / `post_system_prompt` the record cannot
/// use. Python `WRAP_PROMPT_REFUSAL.format(name=…)`.
#[must_use]
pub fn wrap_prompt_refusal(name: &str) -> String {
    format!(
        "{name} must be a non-empty string (it wraps the system block on every turn) — \
         null clears it"
    )
}

/// The record's `pre_system_prompt` / `post_system_prompt` — a non-blank string,
/// else `None` (nothing is added).
#[must_use]
pub fn record_wrap_prompt(meta: &Map<String, Value>, name: &str) -> Option<String> {
    meta.get(name)
        .and_then(Value::as_str)
        .filter(|s| !py_strip(s).is_empty())
        .map(str::to_string)
}

/// The `file_bridge_id` field's hint. Python `AI_FIELDS`.
pub const FILE_BRIDGE_ID_HINT: &str = "the file_bridge this agent reads files THROUGH (its \
     `system_prompt_file`); null = none, nothing is read";

/// The `system_prompt_file` field's hint. Python `AI_FIELDS`.
pub const SYSTEM_PROMPT_FILE_HINT: &str = "a path on `file_bridge_id`, read through it as this \
     agent's system block (placeholders filled), re-read when the bridge reports it changed; a \
     record or per-call `system_prompt` beats it; null = none";

/// The `system_prompt` field's hint. Python `AI_FIELDS`.
pub const SYSTEM_PROMPT_HINT: &str = "a system block that REPLACES the default one on every \
     turn of this agent (a per-call `system_prompt` beats it for that delivery); placeholders \
     {tree} {self} {store} {howto} are filled each turn; null = the default";

/// The refusal for a `system_prompt` the record cannot use. Python
/// `SYSTEM_PROMPT_REFUSAL`.
pub const SYSTEM_PROMPT_REFUSAL: &str = "system_prompt must be a non-empty string (it \
     replaces the default system block on every turn) — null clears it";

/// The record's own `system_prompt`, read from its meta — a non-blank string
/// (python `str.strip()`), else `None` (the default block). Precedence at a turn:
/// the per-call argument > this > the default block.
#[must_use]
pub fn record_system_prompt(meta: &Map<String, Value>) -> Option<String> {
    meta.get("system_prompt")
        .and_then(Value::as_str)
        .filter(|s| !py_strip(s).is_empty())
        .map(str::to_string)
}

/// The `credentials` field of a backend that sends a key. Python
/// `credentials_field(required=…)`.
#[must_use]
pub fn credentials_field(required: bool) -> Field {
    let hint = format!(
        "{{store, ref}}: the credential store and ref holding `api_key` — read at each \
         request, sent only to the ref's host{}",
        if required {
            ""
        } else {
            "; absent = no key (a keyless server)"
        }
    );
    let f = Field::new("credentials", "credentials", hint);
    if required {
        f.required()
    } else {
        f
    }
}

/// A backend's whole record schema: its own fields, then [`ai_fields`] — python
/// `schema(backend_fields)`. A backend field named like a shared one REPLACES it
/// (vllm_backend: `context_window` has no default there — left out, it is read
/// from the server).
///
/// # Panics
/// On a duplicate field name, or a field [`Field::json`] refuses.
#[must_use]
pub fn schema(backend_fields: Vec<Field>) -> Value {
    schema_with_window(backend_fields, DEFAULT_CONTEXT_WINDOW)
}

/// The same, for a backend whose model window is its own
/// ([`crate::agent_loop::BackendConfig::model_window`]): `context_window`
/// defaults to it.
///
/// # Panics
/// On a duplicate field name, or a field [`Field::json`] refuses.
#[must_use]
pub fn schema_with_window(backend_fields: Vec<Field>, context_window: i64) -> Value {
    let own: Vec<&str> = backend_fields.iter().map(|f| f.name).collect();
    let shared: Vec<Field> = ai_fields_with_window(context_window)
        .into_iter()
        .filter(|f| !own.contains(&f.name))
        .collect();
    let fields: Vec<Field> = backend_fields.into_iter().chain(shared).collect();
    let names: Vec<&str> = fields.iter().map(|f| f.name).collect();
    let mut seen = std::collections::HashSet::new();
    assert!(
        names.iter().all(|n| seen.insert(*n)),
        "duplicate field in schema: {names:?}"
    );
    json!({
        "fields": fields.iter().map(Field::json).collect::<Vec<_>>(),
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
/// one of its values, a `credentials` field must be `{store, ref}`, a
/// `system_prompt` it names a non-empty string (or null), and every required field
/// must be present. Every missing field is named at once, each
/// with its hint, so one retry fixes the record.
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
        if str_of(f, "type") == "credentials" {
            if let Some(bad) = shape_error(v) {
                return Some(bad);
            }
            continue;
        }
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
    // `misfire_limit`: named on a write it is an int from 2 to 50 (#203).
    if let Some(v) = meta.get("misfire_limit").filter(|v| !v.is_null()) {
        if !v.as_i64().is_some_and(|n| (2..=50).contains(&n)) {
            return Some("misfire_limit must be an int from 2 to 50".to_string());
        }
    }
    // `transport_retries`: named on a write it is an int from 0 to 5 (#198 part 2).
    if let Some(v) = meta.get("transport_retries").filter(|v| !v.is_null()) {
        if !v.as_i64().is_some_and(|n| (0..=5).contains(&n)) {
            return Some("transport_retries must be an int from 0 to 5".to_string());
        }
    }
    // `system_prompt` (every ai_core agent's): named on a write it is a non-blank
    // string, or null to clear it.
    if let Some(v) = meta.get("system_prompt").filter(|v| !v.is_null()) {
        if !v.as_str().is_some_and(|s| !py_strip(s).is_empty()) {
            return Some(SYSTEM_PROMPT_REFUSAL.to_string());
        }
    }
    for name in ["pre_system_prompt", "post_system_prompt"] {
        if let Some(v) = meta.get(name).filter(|v| !v.is_null()) {
            if !v.as_str().is_some_and(|s| !py_strip(s).is_empty()) {
                return Some(wrap_prompt_refusal(name));
            }
        }
    }
    if let Some(bad) = crate::prompt_file::shape_error(meta) {
        return Some(bad);
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
        schema(vec![
            Field::new("endpoint", "str", "the URL").required(),
            Field::new("mode", "enum", "a or b").one_of(&["a", "b"]),
        ])
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
                "recent_n",
                "caching",
                "cache_primer_until",
                "transport_retries",
                "misfire_limit",
                "system_prompt",
                "file_bridge_id",
                "system_prompt_file",
                "pre_system_prompt",
                "post_system_prompt"
            ]
        );
        // no `key_var`: a key is never read from the environment — a backend
        // that sends one has a `credentials` field
        assert_eq!(
            sch.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["fields"]
        );
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
                "context_strategy=3 is not one of the legal values: compact, truncate, strip, fold, none"
            )
        );
        // null is "not named" — never judged against the set
        assert_eq!(
            validate(&sample(), "x", &obj(json!({"mode": null})), &rec),
            None
        );
    }

    #[test]
    fn a_system_prompt_is_a_non_blank_string_or_null() {
        let sch = sample();
        let rec = obj(json!({"endpoint": "http://h", "memory_store_id": "m"}));
        assert_eq!(
            sch["fields"][11].to_string(),
            json!({"name": "system_prompt", "type": "str", "required": false,
                   "hint": SYSTEM_PROMPT_HINT})
            .to_string()
        );
        // python's words, byte for byte
        assert_eq!(
            SYSTEM_PROMPT_HINT,
            "a system block that REPLACES the default one on every turn of this agent \
             (a per-call `system_prompt` beats it for that delivery); placeholders {tree} \
             {self} {store} {howto} are filled each turn; null = the default"
        );
        assert_eq!(
            SYSTEM_PROMPT_REFUSAL,
            "system_prompt must be a non-empty string (it replaces the default system \
             block on every turn) — null clears it"
        );
        for bad in [
            json!(""),
            json!("  \n"),
            json!(5),
            json!(["x"]),
            json!({"a": 1}),
        ] {
            assert_eq!(
                validate(&sch, "x", &obj(json!({ "system_prompt": bad })), &rec).as_deref(),
                Some(SYSTEM_PROMPT_REFUSAL),
                "{bad}"
            );
        }
        // null clears it; a real string is taken as it is
        assert_eq!(
            validate(&sch, "x", &obj(json!({"system_prompt": null})), &rec),
            None
        );
        assert_eq!(
            validate(&sch, "x", &obj(json!({"system_prompt": "be terse"})), &rec),
            None
        );
        // the record reader: a non-blank string, else the default block
        assert_eq!(
            record_system_prompt(&obj(json!({"system_prompt": "be terse"}))).as_deref(),
            Some("be terse")
        );
        assert_eq!(
            record_system_prompt(&obj(json!({"system_prompt": " "}))),
            None
        );
        assert_eq!(
            record_system_prompt(&obj(json!({"system_prompt": null}))),
            None
        );
        assert_eq!(record_system_prompt(&Map::new()), None);
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
    fn a_backend_field_replaces_the_shared_one_of_its_name() {
        let sch = schema(vec![Field::new("recent_n", "int", "h")]);
        let fields = sch["fields"].as_array().expect("fields");
        let named = fields.iter().filter(|f| f["name"] == "recent_n").count();
        assert_eq!(named, 1);
        assert_eq!(
            fields[0],
            json!({"name": "recent_n", "type": "int", "required": false, "hint": "h"})
        );
    }

    #[test]
    #[should_panic(expected = "duplicate field in schema")]
    fn a_backend_field_named_twice_is_refused() {
        let _ = schema(vec![
            Field::new("x", "int", "h"),
            Field::new("x", "int", "h"),
        ]);
    }

    #[test]
    fn the_credentials_field_is_pythons() {
        assert_eq!(
            credentials_field(true).json(),
            json!({"name": "credentials", "type": "credentials", "required": true,
                   "hint": "{store, ref}: the credential store and ref holding `api_key` — \
                            read at each request, sent only to the ref's host"})
        );
        assert_eq!(
            credentials_field(false).json()["hint"],
            json!(
                "{store, ref}: the credential store and ref holding `api_key` — read at each \
                 request, sent only to the ref's host; absent = no key (a keyless server)"
            )
        );
        assert_eq!(credentials_field(false).json()["required"], json!(false));
    }

    #[test]
    fn a_credentials_value_must_be_store_and_ref() {
        let sch = schema(vec![credentials_field(true)]);
        let rec = obj(json!({"memory_store_id": "m"}));
        let v = |c: Value| validate(&sch, "x", &obj(json!({ "credentials": c })), &rec);
        assert_eq!(
            v(json!({"store": "creds"})).as_deref(),
            Some("credentials must be {store: <credential_store id>, ref: <ref>}")
        );
        assert_eq!(
            v(json!({"store": "creds", "ref": "r", "x": 1})).as_deref(),
            Some("credentials must be {store: <credential_store id>, ref: <ref>}")
        );
        assert_eq!(
            v(json!("creds/r")).as_deref(),
            Some("credentials must be {store: <credential_store id>, ref: <ref>}")
        );
        assert_eq!(
            v(json!({"store": " ", "ref": "r"})).as_deref(),
            Some("credentials.store must be a non-empty string")
        );
        assert_eq!(
            v(json!({"store": "creds", "ref": 5})).as_deref(),
            Some("credentials.ref must be a non-empty string")
        );
        assert_eq!(v(json!({"store": "creds", "ref": "r"})), None);
        // required: its absence is named with its hint
        assert!(validate(&sch, "x", &Map::new(), &rec)
            .is_some_and(|e| e.contains("missing: credentials —")));
    }
}
