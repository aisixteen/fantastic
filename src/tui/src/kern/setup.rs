//! WRITING AN AI AGENT's RECORD, from the set-up page.
//!
//! The page asks the human for the id, the handler (one of the backends this
//! build links, from the kernel's own bundle catalog), and then THE BACKEND'S
//! OWN FIELDS as its declared record names them (`Bundle::record`: name,
//! type, required, enum, the backend's default, a hint) — generic, so a
//! new backend needs no page change. Required fields start empty (nothing is
//! guessed); a field with a default starts at the BACKEND's value. The backend
//! validates at the create — one truth — and its refusal names every missing
//! field with its hint.
//!
//! The record is written AS `user`, its parent (grants come from above): the
//! human may call it, it may call each target it names and the memory store
//! (the root's `memory_store_id`, which admits it); `user`'s own side and each
//! target's `callable_by` are `kern/reach`'s — an AI agent is a holder.

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// Where a new agent goes: under the one user.
pub const PARENT: &str = crate::env::HUMAN;

/// Fields the WRITE sets itself — never asked for on the page. `credentials`
/// names the key stored for the backend at its endpoint (`kern/keys`).
pub const WRITE_OWNED: [&str; 2] = ["memory_store_id", "credentials"];

/// One field of a backend's record schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: String,
    pub required: bool,
    pub one_of: Vec<String>,
    pub default: Option<Value>,
    pub hint: String,
}

/// A linked AI backend and what its record takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    pub module: String,
    /// The page's fields (the write-owned ones left out), in schema order.
    pub fields: Vec<Field>,
    /// Whether its record takes a key (`Some(required)`): the key is stored
    /// with `k` and named on the record by the write (`kern/keys`).
    pub key: Option<bool>,
}

impl Backend {
    /// A backend from its catalog schema.
    pub fn from_schema(module: &str, schema: &Value) -> Self {
        let fields = schema["fields"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        let name = f["name"].as_str()?.to_string();
                        (!WRITE_OWNED.contains(&name.as_str())).then(|| Field {
                            name,
                            ty: f["type"].as_str().unwrap_or("str").to_string(),
                            required: f["required"].as_bool().unwrap_or(false),
                            one_of: f["enum"]
                                .as_array()
                                .map(|e| e.iter().filter_map(Value::as_str).map(str::to_string).collect())
                                .unwrap_or_default(),
                            default: f.get("default").cloned().filter(|v| !v.is_null()),
                            hint: f["hint"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let key = schema["fields"].as_array().and_then(|a| {
            a.iter().find(|f| f["type"] == "credentials").map(|f| f["required"].as_bool().unwrap_or(false))
        });
        Self { module: module.to_string(), fields, key }
    }
}

/// What a page needs of a backend's DECLARED record: `{fields:[{name, type, required,
/// enum?, default?, hint}]}` — the record's fields in declaration order, each one's
/// kind read from its schema (`str`, `int`, `float`, `bool`, `enum`, `list`, `map`;
/// `credentials` for the field naming a stored key).
pub fn schema_of(decl: &fantastic_kernel::RecordDecl) -> Value {
    let schema = decl.schema();
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    // a field's schema, or its non-null alternatives (an optional field is `anyOf` null)
    fn parts(p: &Value) -> Vec<&Value> {
        match p.get("anyOf").or_else(|| p.get("oneOf")).and_then(Value::as_array) {
            Some(alts) => alts.iter().filter(|a| a.get("type").and_then(Value::as_str) != Some("null")).collect(),
            None => vec![p],
        }
    }
    let kind = |name: &str, p: &Value| -> &'static str {
        if name == "credentials" {
            return "credentials";
        }
        for part in parts(p) {
            if part.get("enum").is_some() {
                return "enum";
            }
            let types: Vec<&str> = match part.get("type") {
                Some(Value::String(t)) => vec![t.as_str()],
                Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for t in types {
                match t {
                    "integer" => return "int",
                    "number" => return "float",
                    "boolean" => return "bool",
                    "array" => return "list",
                    "object" => return "map",
                    _ => {}
                }
            }
        }
        "str"
    };
    let fields: Vec<Value> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .map(|(name, p)| {
                    let one_of: Vec<&Value> = parts(p)
                        .into_iter()
                        .filter_map(|part| part.get("enum").and_then(Value::as_array))
                        .flatten()
                        .filter(|v| v.is_string())
                        .collect();
                    let mut f = json!({
                        "name": name,
                        "type": kind(name, p),
                        "required": required.contains(&name.as_str()),
                        "hint": p.get("description").and_then(Value::as_str).unwrap_or(""),
                    });
                    if !one_of.is_empty() {
                        f["enum"] = json!(one_of);
                    }
                    if let Some(d) = p.get("default").filter(|d| !d.is_null()) {
                        f["default"] = d.clone();
                    }
                    f
                })
                .collect()
        })
        .unwrap_or_default();
    json!({"fields": fields})
}

/// The backends this build links — every registered module with a record
/// schema that is one of the five backend names — with their schemas.
pub fn backends(host: &Host) -> Vec<Backend> {
    let mut v: Vec<Backend> = host
        .kernel
        .bundles
        .catalog()
        .filter(|(m, _)| {
            matches!(
                *m,
                "anthropic_backend.tools" | "ollama_backend.tools" | "responses_backend.tools" | "vllm_backend.tools" | "foundation_models_backend.tools"
            )
        })
        .map(|(m, b)| Backend::from_schema(m, &schema_of(b.record())))
        .collect();
    v.sort_by(|a, b| a.module.cmp(&b.module));
    v
}

/// A field's text as the page holds it → the record's value, typed by the
/// schema. Empty text = not set.
pub fn coerce(f: &Field, text: &str) -> Result<Option<Value>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let bad = |what: &str| format!("{}: `{t}` is not {what}", f.name);
    Ok(Some(match f.ty.as_str() {
        "int" => json!(t.parse::<i64>().map_err(|_| bad("an integer"))?),
        "float" => json!(t.parse::<f64>().map_err(|_| bad("a number"))?),
        "bool" => json!(match t {
            "true" | "yes" | "1" => true,
            "false" | "no" | "0" => false,
            _ => return Err(bad("true or false")),
        }),
        "enum" if !f.one_of.iter().any(|o| o == t) => return Err(bad(&format!("one of {}", f.one_of.join("|")))),
        "list" => json!(t.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).collect::<Vec<_>>()),
        "map" => serde_json::from_str::<Value>(t).ok().filter(Value::is_object).ok_or_else(|| bad("a JSON object"))?,
        _ => json!(t),
    }))
}

/// A value as the page shows it (the inverse of [`coerce`]).
pub fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string)).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The record the page wants written.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub id: String,
    pub handler: String,
    /// The backend's own fields that are set, already typed.
    pub fields: Map<String, Value>,
    pub may_call: Vec<String>,
}

impl Record {
    /// What the PAGE must have before a write: the id and the handler. The
    /// backend's own required fields are the backend's to refuse, by name.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.id.trim().is_empty() {
            m.push("id");
        }
        if self.handler.is_empty() {
            m.push("handler");
        }
        m
    }
}

fn err_of(r: &fantastic_kernel::Outcome) -> Option<String> {
    r.result.get("error").and_then(Value::as_str).map(|e| {
        match r.result.get("reason").and_then(Value::as_str) {
            Some(why) => format!("{e} — {why}"),
            None => e.to_string(),
        }
    })
}



/// Field TEXTS (`{name: text}`) typed by `handler`'s record schema — the one
/// typing path for a write and a saved set-up (an empty text is left out).
pub fn typed(host: &Host, handler: &str, texts: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let backend = backends(host)
        .into_iter()
        .find(|b| b.module == handler)
        .ok_or_else(|| format!("this build links no backend {handler:?}"))?;
    let mut fields = Map::new();
    for f in &backend.fields {
        let t = texts.get(&f.name).and_then(Value::as_str).unwrap_or("");
        if let Some(v) = coerce(f, t)? {
            fields.insert(f.name.clone(), v);
        }
    }
    Ok(fields)
}

/// Create, grant and boot. Returns where the record was written.
pub async fn write(host: &Host, rec: &Record) -> Result<String, String> {
    let missing = rec.missing();
    if !missing.is_empty() {
        return Err(format!("missing: {}", missing.join(", ")));
    }
    let id = rec.id.trim();
    if host.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id)) {
        return Err(format!("`{id}` already exists"));
    }
    for t in &rec.may_call {
        if !host.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(t.as_str())) {
            return Err(format!("may call: no agent `{t}`"));
        }
    }
    // the pair: the provider (the backend, its fields and key) and the chat `id` naming it, on the
    // kernel's memory store; the targets the page names are the chat's to call. Both are made
    // under `user` by `make_ai`; `user`'s own side and each target's `callable_by` are `kern/reach`'s
    let peer: Map<String, Value> = rec.may_call.iter().map(|t| (t.clone(), json!("*"))).collect();
    let mut spec = rec.fields.clone();
    spec.insert("handler_module".into(), json!(rec.handler));
    crate::kern::backend::BackendBundle::make_ai(host, &fantastic_kernel::AgentId::from(PARENT), id, spec, peer).await?;
    for agent in [crate::kern::backend::provider_id(id), id.to_string()] {
        let r = host.act(&agent, json!({"type": "boot"})).await;
        if let Some(e) = err_of(&r) {
            return Err(format!("`{agent}` was written but did not boot: {e}"));
        }
    }
    Ok(host.kernel.state_dir()?.join("agents").join(PARENT).join("agents").join(id).join("agent.yaml").display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kern::backend::create_tests::{host, record_of};

    #[tokio::test]
    async fn write_makes_the_provider_and_the_chat_naming_it_both_booted() {
        let (h, dir) = host("setup-write", &["mine", "mine_provider"]).await;
        let mut fields = Map::new();
        fields.insert("endpoint".into(), json!("http://box.example:8000/v1"));
        fields.insert("model".into(), json!("m1"));
        // what `kern/reach` does at each new agent: `user` is granted it (the boot is `user`'s call)
        let weak = std::sync::Arc::downgrade(&h.kernel);
        let cfg = h.cfg.clone();
        h.kernel.add_state_subscriber(std::sync::Arc::new(move |v: &Value, _b: Option<&fantastic_kernel::Blob>| {
            if v.get("kind").and_then(Value::as_str) != Some("added") {
                return;
            }
            if let Some(kernel) = weak.upgrade() {
                let host = Host { kernel, cfg: cfg.clone(), root: "kernel".to_string() };
                let _ = futures_util::FutureExt::now_or_never(crate::kern::reach::grant_all(&host));
            }
        }));
        let rec = Record {
            id: "mine".into(),
            handler: "vllm_backend.tools".into(),
            fields,
            may_call: vec![],
        };
        let at = write(&h, &rec).await.unwrap();
        assert!(at.ends_with("/agents/mine/agent.yaml"), "{at}");
        assert!(at.starts_with(&h.kernel.state_dir().unwrap().display().to_string()), "{at}");
        for id in ["mine", "mine_provider"] {
            assert!(
                h.kernel
                    .agents
                    .contains_key(&fantastic_kernel::AgentId::from(id)),
                "{id} is written"
            );
        }
        let (chat, prov) = (record_of(&h, "mine"), record_of(&h, "mine_provider"));
        assert_eq!(chat["provider_id"], json!("mine_provider"));
        assert!(chat.get("endpoint").is_none(), "the chat holds no endpoint");
        assert_eq!(prov["endpoint"], json!("http://box.example:8000/v1"));
        assert!(prov.get("provider_id").is_none(), "the provider is no chat");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
