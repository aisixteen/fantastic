//! TEXT TYPED FROM A VERB'S SCHEMA — the one place text a caller wrote (a model's
//! `<k>v</k>`, an operator's `k=v`) becomes the value the verb's declaration asks
//! for. Which type a string stands for is the verb's own declaration: a `path` of
//! `007` is the text `007`, a `limit` of `7` is the integer 7. There is no guessing
//! rule and no coercion at the receiving end — the agent's entry
//! ([`crate::verbs::Parse`]) takes only the declared types; a value that is not of
//! the type it should be is left as written and refused there, by name.

use crate::kernel::Kernel;
use crate::verbs::{rows, Verb};
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

/// One `k=v` an OPERATOR typed, as the value the argument's schema asks for: a JSON
/// literal where it asks for a list or an object, else [`typed`] text.
#[must_use]
pub fn operator_text(schema: &Value, text: &str) -> Value {
    let ks = kinds(schema);
    let wants = |k: &str| ks.contains(&k);
    let looks_json = (text.starts_with('{') && text.ends_with('}') && wants("object"))
        || (text.starts_with('[') && text.ends_with(']') && wants("array"));
    if looks_json {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            return typed(schema, &parsed);
        }
    }
    typed(schema, &Value::String(text.to_string()))
}

/// The schema of the verb `verb` of `target`, as an object schema: its `properties` are
/// the verb's declared arguments (the agent's bundle's builtin and dynamic verbs, the
/// verbs every agent answers, the kernel's own on the root) and — for `create_agent` and
/// `update_agent`, whose further arguments are fields of a RECORD — the fields of the
/// record of the agent the call concerns (`handler_module` / `bundle` at create, `id` at
/// update), read from `args`. `None` when the agent or the verb is not declared here (a
/// peer's agent): the text stays as written.
#[must_use]
pub fn verb_schema(
    kernel: &Arc<Kernel>,
    target: &str,
    verb: &str,
    args: &Map<String, Value>,
) -> Option<Value> {
    let agent = kernel.resolve_target(target)?;
    let bundle = agent
        .handler_module
        .as_deref()
        .and_then(|hm| kernel.bundles.get(hm));
    let dynamic = bundle
        .as_ref()
        .map(|b| b.dynamic_verbs(&agent))
        .unwrap_or_default();
    let mut lists: Vec<&[Verb]> = Vec::new();
    if agent.parent_id.is_none() {
        lists.push(crate::declared::kernel::VERBS);
    }
    if let Some(b) = bundle.as_ref() {
        lists.push(b.verbs());
    }
    lists.push(crate::declared::common::VERBS);
    let mut schema = rows(&lists, &dynamic)
        .into_iter()
        .find(|r| r.name == verb)
        .map(|r| r.schema.clone())?;
    if agent.parent_id.is_none() {
        let kind = match verb {
            "create_agent" => args
                .get("bundle")
                .or_else(|| args.get("handler_module"))
                .and_then(Value::as_str)
                .and_then(|hm| kernel.bundles.get(hm)),
            "update_agent" => args
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| kernel.resolve_target(id))
                .and_then(|a| {
                    a.handler_module
                        .as_deref()
                        .and_then(|hm| kernel.bundles.get(hm))
                }),
            _ => None,
        };
        let mut record_props = Map::new();
        for decl in [
            Some(&crate::declared::record::RECORD),
            kind.as_ref().map(|b| b.record()),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(Value::Object(p)) = decl.schema().get("properties") {
                record_props.extend(p.clone());
            }
        }
        if let Some(Value::Object(own)) = schema.get_mut("properties") {
            for (k, v) in record_props {
                own.entry(k).or_insert(v);
            }
        } else if !record_props.is_empty() {
            schema["properties"] = Value::Object(record_props);
        }
    }
    Some(schema)
}

/// The payload of a call to `target`, its values typed by the schema of the verb it
/// names; a target or verb not declared here leaves the text as written.
pub fn payload(kernel: &Arc<Kernel>, target: &str, payload: &mut Map<String, Value>) {
    let Some(verb) = payload
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let args: Map<String, Value> = payload
        .iter()
        .filter(|(k, _)| k.as_str() != "type")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let Some(schema) = verb_schema(kernel, target, &verb, &args) else {
        return;
    };
    if let Value::Object(done) = typed(&schema, &Value::Object(args)) {
        for (k, v) in done {
            payload.insert(k, v);
        }
    }
}

/// The payload of a one-shot call an OPERATOR typed as `k=v` pairs: each value takes the
/// type its argument's declaration asks for ([`operator_text`]); an agent or verb not
/// declared here leaves the text as written, and the target refuses a wrongly typed
/// value by name.
#[must_use]
pub fn operator_payload(
    kernel: &Arc<Kernel>,
    target: &str,
    verb: &str,
    pairs: &[(String, String)],
) -> Map<String, Value> {
    let text: Map<String, Value> = pairs
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let schema = verb_schema(kernel, target, verb, &text);
    let mut out = Map::new();
    out.insert("type".to_string(), Value::String(verb.to_string()));
    for (k, v) in pairs {
        let prop = schema
            .as_ref()
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.get(k))
            .cloned()
            .unwrap_or(Value::Null);
        out.insert(k.clone(), operator_text(&prop, v));
    }
    out
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
        let schema =
            json!({"properties": {"limit": {"type": "integer"}, "force": {"type": "boolean"}}});
        let args = json!({"limit": "many", "force": "True"});
        assert_eq!(typed(&schema, &args), args);
        // a leading zero is not an integer's spelling; an enum of text stays text
        let schema =
            json!({"properties": {"n": {"type": "integer"}, "tier": {"enum": ["1", "2"]}}});
        assert_eq!(
            typed(&schema, &json!({"n": "007", "tier": "1"})),
            json!({"n": "007", "tier": "1"})
        );
    }

    #[test]
    fn the_kernels_own_verbs_are_typed_by_their_declarations_on_the_root() {
        use crate::{Agent, AgentId};
        let mut kernel = Kernel::new();
        crate::test_support::register_root_loader(&mut kernel.bundles);
        crate::test_support::register_fake_store(
            &mut kernel.bundles,
            std::path::Path::new("/nowhere"),
        );
        let kernel = Arc::new(kernel);
        let root = Agent::new(
            AgentId::from("kernel_state"),
            Some("kernel_state.tools".to_string()),
            None,
            Map::new(),
            std::path::PathBuf::new(),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        let pairs = |kv: &[(&str, &str)]| -> Vec<(String, String)> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        // an integer where `list_agents` declares one, text where `docs` declares text
        let p = operator_payload(&kernel, "kernel", "list_agents", &pairs(&[("limit", "7")]));
        assert_eq!(p["limit"], json!(7));
        let p = operator_payload(&kernel, "kernel", "docs", &pairs(&[("of", "7")]));
        assert_eq!(p["of"], json!("7"));
        // a list only where the verb declares one
        let p = operator_payload(
            &kernel,
            "kernel",
            "await_set",
            &pairs(&[("ids", r#"["a","b"]"#), ("timeout_s", "2.5")]),
        );
        assert_eq!(p["ids"], json!(["a", "b"]));
        assert_eq!(p["timeout_s"], json!(2.5));
        // the fields of the record a create or update concerns are typed by ITS declaration
        let p = operator_payload(
            &kernel,
            "kernel",
            "create_agent",
            &pairs(&[
                ("handler_module", "file_bridge.tools"),
                ("allow", r#"["."]"#),
                ("root", "12"),
            ]),
        );
        assert_eq!(p["allow"], json!(["."]));
        assert_eq!(p["root"], json!("12"));
        let p = operator_payload(
            &kernel,
            "kernel",
            "update_agent",
            &pairs(&[("id", "kernel_state"), ("delete_lock", "true")]),
        );
        assert_eq!(p["delete_lock"], json!(true));
        // a verb the agent does not declare types nothing
        let p = operator_payload(&kernel, "kernel", "make_tea", &pairs(&[("limit", "7")]));
        assert_eq!(p["limit"], json!("7"));
        let p = operator_payload(&kernel, "nobody", "list_agents", &pairs(&[("limit", "7")]));
        assert_eq!(p["limit"], json!("7"));
    }

    #[test]
    fn an_operators_text_is_json_only_where_the_schema_asks_for_a_list_or_object() {
        let list = json!({"type": "array", "items": {"type": "integer"}});
        assert_eq!(operator_text(&list, "[1,2]"), json!([1, 2]));
        let text = json!({"type": "string"});
        assert_eq!(operator_text(&text, "[1,2]"), json!("[1,2]"));
        assert_eq!(operator_text(&json!({"type": "integer"}), "42"), json!(42));
        assert_eq!(
            operator_text(&json!({"type": "boolean"}), "true"),
            json!(true)
        );
        assert_eq!(operator_text(&Value::Null, "42"), json!("42"));
    }
}
