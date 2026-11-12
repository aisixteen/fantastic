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
    /// The declared type as the kernel renders it (`string`, `int`, `number`, `bool`,
    /// `list[string]`, `'a'|'b'`, `object`, ...).
    pub ty: String,
    pub required: bool,
    pub one_of: Vec<String>,
    pub default: Option<Value>,
    pub hint: String,
}

/// The options of an enum label (`'a'|'b'`); none for any other type.
fn enum_of(label: &str) -> Vec<String> {
    let opts: Vec<&str> = label.split('|').collect();
    if opts.iter().all(|o| o.len() >= 2 && o.starts_with('\'') && o.ends_with('\'')) {
        opts.iter().map(|o| o[1..o.len() - 1].to_string()).collect()
    } else {
        Vec::new()
    }
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
                            one_of: enum_of(f["type"].as_str().unwrap_or("")),
                            ty: f["type"].as_str().unwrap_or("string").to_string(),
                            required: f["required"].as_bool().unwrap_or(false),
                            default: f.get("default").cloned().filter(|v| !v.is_null()),
                            hint: f["hint"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let key = schema["fields"].as_array().and_then(|a| {
            a.iter().find(|f| f["name"] == "credentials").map(|f| f["required"].as_bool().unwrap_or(false))
        });
        Self { module: module.to_string(), fields, key }
    }
}

/// The AI backend bundles a set-up can name.
pub const BACKENDS: [&str; 5] =
    ["anthropic_backend.tools", "ollama_backend.tools", "responses_backend.tools", "vllm_backend.tools", "foundation_models_backend.tools"];

/// The backends this build links — every registered module with a record
/// schema that is one of the five backend names — with their schemas.
pub fn backends(host: &Host) -> Vec<Backend> {
    let mut v: Vec<Backend> = host
        .kernel
        .bundles
        .catalog()
        .filter(|(m, _)| BACKENDS.contains(m))
        .map(|(m, b)| Backend::from_schema(m, &fantastic_kernel::verbs::record_fields(b.record())))
        .collect();
    v.sort_by(|a, b| a.module.cmp(&b.module));
    v
}

/// What the TUI form's text box holds for a field → the value the record is sent with.
/// A `string` or an enum is the text itself; any other declared type is a JSON literal
/// (a number, `true`, a list, an object) read as written — nothing is converted across
/// types, and the record's own declaration refuses a value of the wrong type at the
/// write. Empty text = not set.
pub fn literal(f: &Field, text: &str) -> Result<Option<Value>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if f.ty == "string" || !f.one_of.is_empty() {
        return Ok(Some(Value::String(t.to_string())));
    }
    serde_json::from_str::<Value>(t)
        .map(Some)
        .map_err(|_| format!("{}: `{t}` is not a JSON value ({})", f.name, f.ty))
}

/// A value as the page shows it (the inverse of [`literal`]).
pub fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
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



/// The TUI form's field TEXTS (`{name: text}`) read as values by `handler`'s declared
/// field types (an empty text is left out). Only the form calls this: the verbs take
/// typed values.
pub fn typed(host: &Host, handler: &str, texts: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let backend = backends(host)
        .into_iter()
        .find(|b| b.module == handler)
        .ok_or_else(|| format!("this build links no backend {handler:?}"))?;
    let mut fields = Map::new();
    for f in &backend.fields {
        let t = texts.get(&f.name).and_then(Value::as_str).unwrap_or("");
        if let Some(v) = literal(f, t)? {
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
