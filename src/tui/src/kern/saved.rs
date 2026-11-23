//! THE AI AGENT'S SAVED SET-UPS, through the one agent that holds them.
//!
//! `home` is the user's home store under `user` (the blueprint's record). The
//! binary fixes where its files live (`FANTASTIC_HOME`, else `~/.fantastic`;
//! `kern/host` registry). The shell calls it as the operator.
//!
//! A saved set-up is `handler_module` + that bundle's config: for an AI agent the
//! backend, model and endpoint. It never carries an id, a memory store or
//! lines — whoever creates an agent from it adds those. Nothing is a default
//! until the human makes one (`prefer`).

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// The home store's agent id (the blueprint's).
pub const HOME_ID: &str = "home";

/// One saved set-up.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub name: String,
    pub version: String,
    pub record: Map<String, Value>,
}

impl Saved {
    fn field(&self, k: &str) -> String {
        self.record.get(k).and_then(Value::as_str).unwrap_or("").to_string()
    }
    pub fn handler(&self) -> String {
        self.field("handler_module")
    }
    pub fn model(&self) -> String {
        self.field("model")
    }
    pub fn endpoint(&self) -> String {
        self.field("endpoint")
    }
}

/// Everything the store holds, and where.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shelf {
    pub saved: Vec<Saved>,
    pub preferred: Option<String>,
    pub dir: String,
}

fn err_of(v: &Value) -> Option<String> {
    v.get("error").and_then(Value::as_str).map(|e| match v.get("hint").and_then(Value::as_str) {
        Some(h) => format!("{e} — {h}"),
        None => e.to_string(),
    })
}

/// What is saved.
pub async fn shelf(host: &Host) -> Result<Shelf, String> {
    let r = host.operator(HOME_ID, json!({"type": "load_tree"})).await.result;
    if let Some(e) = err_of(&r) {
        return Err(e);
    }
    let saved = r["saved"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            Some(Saved {
                name: t.get("name")?.as_str()?.to_string(),
                version: t.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
                record: t.get("record")?.as_object()?.clone(),
            })
        })
        .collect();
    Ok(Shelf {
        saved,
        preferred: r["preferred"].as_str().map(str::to_string),
        dir: r["dir"].as_str().unwrap_or("").to_string(),
    })
}

/// Save `record` as saved set-up `name`. Returns where it was written.
pub async fn save(host: &Host, name: &str, record: Value) -> Result<String, String> {
    let r = host.operator(HOME_ID, json!({"type": "persist_record", "name": name, "record": record})).await.result;
    match err_of(&r) {
        Some(e) => Err(e),
        None => Ok(r["path"].as_str().unwrap_or("").to_string()),
    }
}

/// Make saved set-up `name` the default for new AI agents; `None` clears it.
pub async fn prefer(host: &Host, name: Option<&str>) -> Result<Option<String>, String> {
    let r = host.operator(HOME_ID, json!({"type": "prefer", "name": name})).await.result;
    match err_of(&r) {
        Some(e) => Err(e),
        None => Ok(r["preferred"].as_str().map(str::to_string)),
    }
}
