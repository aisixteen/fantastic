//! THE USER'S TEMPLATES, through the one agent that holds them.
//!
//! `user_home` is a `user_home_store` agent under the root. The binary fixes
//! where its files live (`FANTASTIC_HOME`, else `~/.config/fantastic`) and that
//! it serves only the human (`kern/host` registry); this module makes sure the
//! agent exists and that the grants between it and the human are exactly the
//! verbs used here — RE-SET on every boot, so a record on disk that widened them
//! is narrowed again.
//!
//! A template is `handler_module` + that bundle's config: for an AI agent the
//! backend, model and endpoint. It never carries an id, a memory store or
//! grants — whoever creates an agent from it adds those. Nothing is a default
//! until the human makes one (`prefer`).

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// The template store's agent id.
pub const HOME_ID: &str = "user_home";

/// Exactly the verbs the human calls on it (three are privileged: named).
const VERBS: [&str; 5] = ["persist_record", "forget_record", "load_tree", "prefer", "reflect"];

/// One saved template.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub name: String,
    pub version: String,
    pub record: Map<String, Value>,
}

impl Template {
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
    pub templates: Vec<Template>,
    pub preferred: Option<String>,
    pub dir: String,
}

fn err_of(v: &Value) -> Option<String> {
    v.get("error").and_then(Value::as_str).map(|e| match v.get("hint").and_then(Value::as_str) {
        Some(h) => format!("{e} — {h}"),
        None => e.to_string(),
    })
}

/// Make `user_home` exist under `user`, its `callable_by` exactly [`VERBS`] for
/// `user` — written as `user`, its parent. (`user`'s own side is `kern/reach`'s.)
pub async fn ensure(host: &Host) -> Result<(), String> {
    let user = host.cfg.user.clone();
    let id = fantastic_kernel::AgentId::from(HOME_ID);
    let grant = json!({ user.as_str(): VERBS });
    if !host.kernel.agents.contains_key(&id) {
        let r = host
            .act(
                "kernel",
                json!({"type": "create_agent", "parent_id": host.cfg.user.as_str(), "id": HOME_ID,
                       "handler_module": fantastic_user_home_store::HANDLER_MODULE, "callable_by": grant}),
            )
            .await;
        if let Some(e) = err_of(&r.result) {
            return Err(format!("could not create `{HOME_ID}`: {e}"));
        }
    } else {
        let r = host.act("kernel", json!({"type": "update_agent", "id": HOME_ID, "callable_by": grant})).await;
        if let Some(e) = err_of(&r.result) {
            return Err(format!("could not narrow `{HOME_ID}`'s grants: {e}"));
        }
    }
    Ok(())
}

/// What is saved.
pub async fn shelf(host: &Host) -> Result<Shelf, String> {
    let r = host.act(HOME_ID, json!({"type": "load_tree"})).await.result;
    if let Some(e) = err_of(&r) {
        return Err(e);
    }
    let templates = r["templates"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            Some(Template {
                name: t.get("name")?.as_str()?.to_string(),
                version: t.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
                record: t.get("record")?.as_object()?.clone(),
            })
        })
        .collect();
    Ok(Shelf {
        templates,
        preferred: r["preferred"].as_str().map(str::to_string),
        dir: r["dir"].as_str().unwrap_or("").to_string(),
    })
}

/// Save `record` as template `name`. Returns where it was written.
pub async fn save(host: &Host, name: &str, record: Value) -> Result<String, String> {
    let r = host.act(HOME_ID, json!({"type": "persist_record", "name": name, "record": record})).await.result;
    match err_of(&r) {
        Some(e) => Err(e),
        None => Ok(r["path"].as_str().unwrap_or("").to_string()),
    }
}

/// Make template `name` the default for new AI agents; `None` clears it.
pub async fn prefer(host: &Host, name: Option<&str>) -> Result<Option<String>, String> {
    let r = host.act(HOME_ID, json!({"type": "prefer", "name": name})).await.result;
    match err_of(&r) {
        Some(e) => Err(e),
        None => Ok(r["preferred"].as_str().map(str::to_string)),
    }
}
