//! The parser's values, TYPED FROM THE TARGET VERB'S SCHEMA.
//!
//! The grammar reads text: a leaf is the string the model wrote. Which type a
//! string stands for is the target verb's own declaration — a `path` of `007` is
//! the text `007`, a `limit` of `7` is the integer 7. [`payload`] walks a call's
//! payload against the declared schema of the verb it names (a builtin verb of the
//! target's bundle, a dynamic verb of that agent, or one of the kernel's own) and
//! produces each value in the type the schema states; a string where the schema
//! allows text stays text. A value that is not of the type it should be is left as
//! written: the target's own validation refuses it by name.

use fantastic_kernel::verbs::rows;
use fantastic_kernel::{Kernel, Verb};
use serde_json::{Map, Number, Value};
use std::sync::Arc;

/// What a schema's value kinds are, anyOf/oneOf/type-arrays flattened. Empty: any.
fn kinds(schema: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    if let Some(Value::Array(vals)) = schema.get("enum") {
        if vals.iter().any(Value::is_string) {
            out.push("string");
        }
        return out;
    }
    match schema.get("type") {
        Some(Value::String(t)) => out.push(t.as_str()),
        Some(Value::Array(ts)) => out.extend(ts.iter().filter_map(Value::as_str)),
        _ => {}
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(alts)) = schema.get(key) {
            for a in alts {
                out.extend(kinds(a));
            }
        }
    }
    out
}

/// The text `s` as the value the schema asks for, or `None` when the schema takes
/// text (or any value) or `s` does not read as what it asks for.
fn scalar(schema: &Value, s: &str) -> Option<Value> {
    let ks = kinds(schema);
    if ks.is_empty() || ks.contains(&"string") {
        return None;
    }
    let t = s.trim();
    if ks.contains(&"boolean") {
        match t {
            "true" => return Some(Value::Bool(true)),
            "false" => return Some(Value::Bool(false)),
            _ => {}
        }
    }
    if ks.contains(&"integer") && is_integer(t) {
        // exact digits, any width (the workspace keeps big integers whole)
        return serde_json::from_str::<Number>(t).ok().map(Value::Number);
    }
    if ks.contains(&"number") {
        if is_integer(t) {
            return serde_json::from_str::<Number>(t).ok().map(Value::Number);
        }
        if let Ok(f) = t.parse::<f64>() {
            return Number::from_f64(f).map(Value::Number);
        }
    }
    None
}

fn is_integer(t: &str) -> bool {
    let digits = t.strip_prefix('-').unwrap_or(t);
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
}

/// `v` typed by `schema`: leaves by [`scalar`], a list by its `items`, an object by
/// its `properties`.
#[must_use]
pub fn typed(schema: &Value, v: &Value) -> Value {
    match v {
        Value::String(s) => scalar(schema, s).unwrap_or_else(|| v.clone()),
        Value::Array(items) => {
            let it = schema.get("items").cloned().unwrap_or(Value::Null);
            Value::Array(items.iter().map(|x| typed(&it, x)).collect())
        }
        Value::Object(m) => {
            let props = schema.get("properties").and_then(Value::as_object);
            Value::Object(
                m.iter()
                    .map(|(k, x)| {
                        let p = props.and_then(|p| p.get(k)).cloned().unwrap_or(Value::Null);
                        (k.clone(), typed(&p, x))
                    })
                    .collect::<Map<String, Value>>(),
            )
        }
        other => other.clone(),
    }
}

/// The routing fields a call carries that are not text: how long to wait and how
/// often to retry. Every other routing field (`target_id`, `reply_to`, `title`,
/// `exec_id`, …) is text.
#[must_use]
pub fn routing(name: &str, v: &Value) -> Value {
    let schema = match name {
        "timeout" => serde_json::json!({"type": "number"}),
        "retries" => serde_json::json!({"type": "integer"}),
        _ => return v.clone(),
    };
    typed(&schema, v)
}

/// Type the payload of a call to `target` by the schema of the verb it names. A
/// target that is not here (a peer's agent, no bundle) or a verb it does not
/// declare leaves the text as written.
pub fn payload(kernel: &Arc<Kernel>, target: &str, payload: &mut Map<String, Value>) {
    let Some(verb) = payload.get("type").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let schema = verb_schema(kernel, target, &verb);
    let Some(schema) = schema else { return };
    let args: Map<String, Value> = payload
        .iter()
        .filter(|(k, _)| k.as_str() != "type")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Value::Object(done) = typed(&schema, &Value::Object(args)) {
        for (k, v) in done {
            payload.insert(k, v);
        }
    }
}

fn verb_schema(kernel: &Arc<Kernel>, target: &str, verb: &str) -> Option<Value> {
    let agent = kernel.resolve_target(target)?;
    let bundle = agent
        .handler_module
        .as_deref()
        .and_then(|hm| kernel.bundles.get(hm))?;
    let dynamic = bundle.dynamic_verbs(&agent);
    let mut lists: Vec<&[Verb]> = Vec::new();
    if agent.parent_id.is_none() {
        lists.push(fantastic_kernel::declared::kernel::VERBS);
    }
    lists.push(bundle.verbs());
    lists.push(fantastic_kernel::declared::common::VERBS);
    rows(&lists, &dynamic)
        .into_iter()
        .find(|r| r.name == verb)
        .map(|r| r.schema.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_becomes_the_type_the_schema_states() {
        let schema = json!({"properties": {
            "limit": {"type": ["integer", "null"]},
            "ratio": {"type": "number"},
            "force": {"type": "boolean"},
            "path": {"type": "string"},
            "ids": {"type": "array", "items": {"type": "integer"}},
        }});
        let args = json!({"limit": "7", "ratio": "1.5", "force": "true", "path": "007",
                          "ids": ["1", "2"]});
        assert_eq!(
            typed(&schema, &args),
            json!({"limit": 7, "ratio": 1.5, "force": true, "path": "007", "ids": [1, 2]})
        );
    }

    #[test]
    fn text_that_is_not_the_type_stays_text_for_the_target_to_refuse() {
        let schema = json!({"properties": {"limit": {"type": "integer"}, "force": {"type": "boolean"}}});
        let args = json!({"limit": "many", "force": "True"});
        assert_eq!(typed(&schema, &args), args);
        // a leading zero is not an integer's spelling; an enum of text stays text
        let schema = json!({"properties": {"n": {"type": "integer"}, "tier": {"enum": ["1", "2"]}}});
        assert_eq!(
            typed(&schema, &json!({"n": "007", "tier": "1"})),
            json!({"n": "007", "tier": "1"})
        );
    }

    #[test]
    fn only_timeout_and_retries_are_typed_in_routing() {
        assert_eq!(routing("timeout", &json!("2.5")), json!(2.5));
        assert_eq!(routing("retries", &json!("3")), json!(3));
        assert_eq!(routing("reply_to", &json!("1001")), json!("1001"));
    }
}
