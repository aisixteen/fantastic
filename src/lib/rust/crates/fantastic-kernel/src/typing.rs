//! THE PARSER'S TEXT READER — where a text surface (a model's `<k>v</k>`, an operator's
//! `k=v`) becomes the value the verb's schema declares. The surface has only text; the
//! declaration says what each argument is, so the reader produces exactly that type or
//! REFUSES the call by argument name: a `limit` of `7` is the integer 7, a `path` of
//! `007` is the text `007`, a `limit` of `many` is a refusal. There is no guessing and no
//! second try — the agent's entry ([`crate::verbs::Parse`]) takes only declared types.

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

/// The text `s` as the value the schema declares: text stays text where the schema
/// takes text (or states nothing), a number or boolean is read where it declares one.
/// `Err(want)` names the declared kind the text is not.
fn read(schema: &Value, s: &str) -> Result<Value, String> {
    let ks = kinds(schema);
    if ks.is_empty() || ks.contains(&"string") {
        return Ok(Value::String(s.to_string()));
    }
    let t = s.trim();
    if ks.contains(&"boolean") {
        match t {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            _ => {}
        }
    }
    if ks.contains(&"integer") && is_integer(t) {
        // exact digits, any width (the workspace keeps big integers whole)
        if let Ok(n) = serde_json::from_str::<Number>(t) {
            return Ok(Value::Number(n));
        }
    }
    if ks.contains(&"number") {
        if is_integer(t) {
            if let Ok(n) = serde_json::from_str::<Number>(t) {
                return Ok(Value::Number(n));
            }
        }
        if let Some(n) = t.parse::<f64>().ok().and_then(Number::from_f64) {
            return Ok(Value::Number(n));
        }
    }
    Err(ks.join(" or "))
}

fn is_integer(t: &str) -> bool {
    let digits = t.strip_prefix('-').unwrap_or(t);
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
}

/// `v`, a value the text surface produced for the argument `name`, read by `schema`:
/// leaves by [`read`], a list by its `items`, an object by its `properties`.
fn read_value(verb: &str, name: &str, schema: &Value, v: &Value) -> Result<Value, Value> {
    match v {
        Value::String(s) => read(schema, s).map_err(|want| crate::refusal::bad(verb, name, &want)),
        Value::Array(items) => {
            let it = schema.get("items").cloned().unwrap_or(Value::Null);
            items
                .iter()
                .map(|x| read_value(verb, name, &it, x))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        Value::Object(m) => {
            let props = schema.get("properties").and_then(Value::as_object);
            let mut out = Map::new();
            for (k, x) in m {
                let p = props.and_then(|p| p.get(k)).cloned().unwrap_or(Value::Null);
                out.insert(k.clone(), read_value(verb, k, &p, x)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// One `k=v` an OPERATOR typed, as the value the argument's schema declares: a JSON
/// literal where it declares a list or an object, else the [`read`] text.
fn operator_text(verb: &str, name: &str, schema: &Value, text: &str) -> Result<Value, Value> {
    let ks = kinds(schema);
    let wants = |k: &str| ks.contains(&k);
    if (wants("object") && !wants("string")) || (wants("array") && !wants("string")) {
        let parsed = serde_json::from_str::<Value>(text)
            .map_err(|_| crate::refusal::bad(verb, name, &ks.join(" or ")))?;
        return read_value(verb, name, schema, &parsed);
    }
    read_value(verb, name, schema, &Value::String(text.to_string()))
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

/// The payload of a call to `target`, its values read by the schema of the verb it
/// names; a target or verb not declared here leaves the text as written. `Err` is the
/// refusal naming the argument that is not what its declaration asks for.
pub fn payload(
    kernel: &Arc<Kernel>,
    target: &str,
    payload: &mut Map<String, Value>,
) -> Result<(), Value> {
    let Some(verb) = payload
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    let args: Map<String, Value> = payload
        .iter()
        .filter(|(k, _)| k.as_str() != "type")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let Some(schema) = verb_schema(kernel, target, &verb, &args) else {
        return Ok(());
    };
    if let Value::Object(done) = read_value(&verb, "", &schema, &Value::Object(args))? {
        for (k, v) in done {
            payload.insert(k, v);
        }
    }
    Ok(())
}

/// A routing field of a call (not an argument of its verb) read as `kind`
/// (`"number"` or `"integer"`).
pub fn routing(name: &str, kind: &str, v: &Value) -> Result<Value, Value> {
    read_value("send", name, &serde_json::json!({ "type": kind }), v)
}

/// The payload of a one-shot call an OPERATOR typed as `k=v` pairs: each value is read as
/// its argument's declaration asks ([`operator_text`]); an agent or verb not declared
/// here leaves the text as written. `Err` names the argument that is not its type.
pub fn operator_payload(
    kernel: &Arc<Kernel>,
    target: &str,
    verb: &str,
    pairs: &[(String, String)],
) -> Result<Map<String, Value>, Value> {
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
        out.insert(k.clone(), operator_text(verb, k, &prop, v)?);
    }
    Ok(out)
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
            read_value("v", "", &schema, &args).unwrap(),
            json!({"limit": 7, "ratio": 1.5, "force": true, "path": "007", "ids": [1, 2]})
        );
    }

    #[test]
    fn text_that_is_not_the_type_is_refused_by_argument_name() {
        let schema =
            json!({"properties": {"limit": {"type": "integer"}, "force": {"type": "boolean"}}});
        let e = read_value("v", "", &schema, &json!({"limit": "many"})).unwrap_err();
        assert_eq!(e["arg"], json!("limit"));
        assert_eq!(e["reason"], json!("bad_arg"));
        assert!(read_value("v", "", &schema, &json!({"force": "True"})).is_err());
        // a leading zero is not an integer's spelling; an enum of text stays text
        let schema =
            json!({"properties": {"n": {"type": "integer"}, "tier": {"enum": ["1", "2"]}}});
        assert!(read_value("v", "", &schema, &json!({"n": "007"})).is_err());
        assert_eq!(
            read_value("v", "", &schema, &json!({"tier": "1"})).unwrap(),
            json!({"tier": "1"})
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
        let p =
            operator_payload(&kernel, "kernel", "list_agents", &pairs(&[("limit", "7")])).unwrap();
        assert_eq!(p["limit"], json!(7));
        let p = operator_payload(&kernel, "kernel", "docs", &pairs(&[("of", "7")])).unwrap();
        assert_eq!(p["of"], json!("7"));
        // a list only where the verb declares one
        let p = operator_payload(
            &kernel,
            "kernel",
            "await_set",
            &pairs(&[("ids", r#"["a","b"]"#), ("timeout_s", "2.5")]),
        )
        .unwrap();
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
        )
        .unwrap();
        assert_eq!(p["allow"], json!(["."]));
        assert_eq!(p["root"], json!("12"));
        let p = operator_payload(
            &kernel,
            "kernel",
            "update_agent",
            &pairs(&[("id", "kernel_state"), ("delete_lock", "true")]),
        )
        .unwrap();
        assert_eq!(p["delete_lock"], json!(true));
        // a verb the agent does not declare types nothing
        let p = operator_payload(&kernel, "kernel", "make_tea", &pairs(&[("limit", "7")])).unwrap();
        assert_eq!(p["limit"], json!("7"));
        let p =
            operator_payload(&kernel, "nobody", "list_agents", &pairs(&[("limit", "7")])).unwrap();
        assert_eq!(p["limit"], json!("7"));
    }

    #[test]
    fn an_operators_text_is_json_only_where_the_schema_asks_for_a_list_or_object() {
        let list = json!({"type": "array", "items": {"type": "integer"}});
        assert_eq!(
            operator_text("v", "a", &list, "[1,2]").unwrap(),
            json!([1, 2])
        );
        assert!(operator_text("v", "a", &list, "one").is_err());
        let text = json!({"type": "string"});
        assert_eq!(
            operator_text("v", "a", &text, "[1,2]").unwrap(),
            json!("[1,2]")
        );
        assert_eq!(
            operator_text("v", "a", &json!({"type": "integer"}), "42").unwrap(),
            json!(42)
        );
        assert_eq!(
            operator_text("v", "a", &json!({"type": "boolean"}), "true").unwrap(),
            json!(true)
        );
        assert_eq!(
            operator_text("v", "a", &Value::Null, "42").unwrap(),
            json!("42")
        );
    }
}
