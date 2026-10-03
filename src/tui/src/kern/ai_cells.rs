//! AI CELLS, host side: the `ai_cells` agent under the canvas's socket leg.
//!
//! The canvas makes an AI cell wherever you type something that is not a known
//! kind (design: `tmp/canvas_ai.md`). The cell's AI agent is built from one of
//! the user's TEMPLATES, and only the CLI may read those (`kern/templates`), so
//! the page asks this agent — it never reads the template store itself.
//!
//! A spawned cell's backend is a small subtree under the leg:
//!
//! ```text
//!   canvas_ws
//!   └── aimem_<hex>   ram_store     the conversation, in RAM (EPHEMERAL)
//!       └── ai_<hex>  <template's handler_module>, memory_store_id: aimem_<hex>
//! ```
//!
//! The store comes FIRST (an AI's boot checks the store it names) and is the
//! subtree's ROOT: deleting it deletes the AI with it — the cell's ✕. The view
//! calls the AI as `user` (a tab with no token, through the users gateway), so
//! the AI accepts the chat verbs from it and `user` is granted them BY NAME,
//! at once — `kern/reach` widens them to every verb afterwards, asynchronously.
//!
//! Verbs: `presets` (template names, their backend and model, and the default),
//! `spawn {template?, stamp?, peer_grants?}` (the named template, else the
//! owner's default — no default and no name is refused, naming how to set one).
//!
//! The host knows NOTHING of the page's composition. `stamp` is the CALLER's:
//! record fields it wants on the cell's backend root (the canvas stamps its view
//! and canvas ids, to reconcile its own pairs), copied without being read.
//! `peer_grants` is the AI's egress to agents on ANOTHER kernel — only
//! `<guid>|<agent>` keys (the canvas grants `<page guid>|canvas: [around, …]`),
//! merged into its `can_call`.
//!
//! KEPT DIALOGS (phase 2). `keep {id, name?}` copies the cell's rows from its RAM
//! store into the project's SQLite memory store under the AI's own id, admits
//! the AI there and switches its `memory_store_id` — the dialog now outlives the
//! cell. Kept dialogs are listed on THIS agent's record (`dialogs: {id: {name,
//! template, record}}`), named by their AI id until renamed (`rename`).
//! `reopen {id, stamp?, peer_grants?}` binds a new cell to the dialog: the live
//! AI when another cell has it open (ONE dialog in several cells, updating
//! live), else a fresh AI with the same id on the SQLite store — its history is
//! there. `dialogs` lists the kept ones. Which cells show an AI is the PAGE's
//! count: it deletes the backend when its last cell on it closes.

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::env::ClientCfg;
use crate::kern::host::Host;

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "ai_cells.tools";

/// The agent's id (created by `fantastic canvas`, under the canvas's leg).
pub const AI_CELLS_ID: &str = "ai_cells";

/// Who the cell's view calls the AI as: a tokenless tab's user.
const CHAT_USER: &str = fantastic_canvas_users::DEFAULT_USER;
/// What the cell's view may call on a spawned AI.
pub const CHAT_VERBS: [&str; 5] = ["send", "history", "status", "watch", "reflect"];

/// A template-record field the dialog snapshot never carries (identity, wiring,
/// authority — added again when the AI is re-made).
const NOT_SNAPSHOT: [&str; 7] = ["id", "parent_id", "guid", "agents", "memory_store_id", "can_call", "callable_by"];

/// Fields a caller's `stamp` may never set: identity, placement, authority.
const NOT_STAMPABLE: [&str; 11] = [
    "type", "id", "parent_id", "guid", "handler_module", "agents", "can_call", "callable_by", "operator", "admins",
    "memory_store_id",
];

/// The caller's `stamp`: an object of plain fields, none of them identity or
/// authority. Absent = none.
fn stamp_of(p: &Value) -> Result<Map<String, Value>, Value> {
    match p.get("stamp") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => match m.keys().find(|k| NOT_STAMPABLE.contains(&k.as_str())) {
            Some(k) => Err(refuse("bad_arg", format!("stamp: `{k}` is not the caller's to set"), "stamp only fields of your own")),
            None => Ok(m.clone()),
        },
        Some(_) => Err(refuse("bad_arg", "stamp: must be an object", "")),
    }
}

/// The caller's `peer_grants`: egress to agents on ANOTHER kernel only — every
/// key `<guid>|<agent>`. Absent = none.
fn peer_grants_of(p: &Value) -> Result<Map<String, Value>, Value> {
    match p.get("peer_grants") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => match m.keys().find(|k| !k.contains('|')) {
            Some(k) => Err(refuse("bad_arg", format!("peer_grants: `{k}` is not an agent on another kernel"), "keys are `<guid>|<agent>`")),
            None => Ok(m.clone()),
        },
        Some(_) => Err(refuse("bad_arg", "peer_grants: must be an object", "")),
    }
}

/// The bundle.
#[derive(Default)]
pub struct AiCellsBundle;

fn refuse(reason: &str, error: impl Into<String>, hint: &str) -> Value {
    json!({"error": error.into(), "reason": reason, "hint": hint})
}

fn hex8() -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl AiCellsBundle {
    fn host(kernel: &Arc<Kernel>) -> Option<Host> {
        let root = kernel.root()?.id.0.clone();
        Some(Host { kernel: Arc::clone(kernel), cfg: ClientCfg::here(), root })
    }

    async fn presets(host: &Host) -> Value {
        match crate::kern::templates::shelf(host).await {
            Ok(sh) => json!({
                "templates": sh.templates.iter().map(|t| json!({
                    "name": t.name, "handler_module": t.handler(), "model": t.model(),
                })).collect::<Vec<_>>(),
                "preferred": sh.preferred,
            }),
            Err(e) => refuse("not_found", e, "the user's templates are not readable"),
        }
    }

    async fn spawn(host: &Host, me: &AgentId, p: &Value) -> Value {
        let stamp = match stamp_of(p) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let peer = match peer_grants_of(p) {
            Ok(g) => g,
            Err(e) => return e,
        };
        let shelf = match crate::kern::templates::shelf(host).await {
            Ok(s) => s,
            Err(e) => return refuse("not_found", e, "the user's templates are not readable"),
        };
        let name = match p.get("template").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(n) => n.to_string(),
            None => match &shelf.preferred {
                Some(n) => n.clone(),
                None => {
                    return refuse(
                        "not_found",
                        "no default AI template",
                        "open ⚙ AI set-up: save a template and make it your default",
                    )
                }
            },
        };
        let Some(t) = shelf.templates.iter().find(|t| t.name == name) else {
            return refuse("not_found", format!("no template `{name}`"), "open ⚙ AI set-up to see your templates");
        };
        let leg = host
            .kernel
            .agents
            .get(me)
            .and_then(|a| a.value().record().parent_id)
            .unwrap_or_default();
        let hex = hex8();
        let (store, ai) = (format!("aimem_{hex}"), format!("ai_{hex}"));
        // 1 · the store — the subtree's root, carrying the caller's stamp
        let mut rec = stamp;
        rec.extend(
            json!({"type": "create_agent", "parent_id": leg, "id": store,
                   "handler_module": fantastic_ram_store::HANDLER_MODULE,
                   // the operator reads its rows back when the dialog is kept
                   "operator": true, "admins": ["kernel"]})
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        let r = host.operator("kernel", Value::Object(rec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("spawn: the cell's store: {e}"), "");
        }
        // 2 · the AI, from the template — identity, wiring and grants added here
        let mut spec: Map<String, Value> = t.record.clone();
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(store));
        spec.insert("id".into(), json!(ai));
        spec.insert("memory_store_id".into(), json!(store));
        spec.insert("template".into(), json!(name));
        spec.insert("callable_by".into(), json!({ CHAT_USER: CHAT_VERBS }));
        let mut cc = peer;
        cc.insert(store.clone(), json!("*"));
        spec.insert("can_call".into(), Value::Object(cc));
        if let Err(e) = crate::kern::keys::attach(host, &t.handler(), &mut spec) {
            let _ = host.operator("kernel", json!({"type": "delete_agent", "id": store})).await;
            return refuse("bad_arg", format!("spawn: the AI from template `{name}`: {e}"), "");
        }
        let named = spec.clone();
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            let _ = host.operator("kernel", json!({"type": "delete_agent", "id": store})).await;
            return refuse("bad_arg", format!("spawn: the AI from template `{name}`: {e}"), "");
        }
        if let Err(e) = crate::kern::keys::wire_record(host, &ai, &named).await {
            let _ = host.operator("kernel", json!({"type": "delete_agent", "id": store})).await;
            return refuse("bad_arg", format!("spawn: {e}"), "");
        }
        // 3 · `user` reaches the AI BY NAME, at once. Grants for cells that are
        // gone are pruned here, so they never pile up.
        if let Err(e) = Self::grant_user(host, &ai).await {
            let _ = host.operator("kernel", json!({"type": "delete_agent", "id": store})).await;
            return refuse("bad_arg", format!("spawn: {e}"), "");
        }
        json!({"id": ai, "root": store, "template": name, "handler_module": t.handler(), "model": t.model()})
    }

    fn leg(host: &Host, me: &AgentId) -> String {
        host.kernel.agents.get(me).and_then(|a| a.value().record().parent_id).unwrap_or_default()
    }

    fn meta_of(host: &Host, id: &str) -> Option<Map<String, Value>> {
        host.kernel.agents.get(&AgentId::from(id)).map(|a| a.value().record().meta.clone())
    }

    fn dialogs_map(host: &Host, me: &AgentId) -> Map<String, Value> {
        Self::meta_of(host, &me.0).and_then(|m| m.get("dialogs").and_then(Value::as_object).cloned()).unwrap_or_default()
    }

    async fn save_dialogs(host: &Host, me: &AgentId, d: Map<String, Value>) -> Result<(), String> {
        let r = host.operator("kernel", json!({"type": "update_agent", "id": me.0, "dialogs": d})).await.result;
        match r.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("ai_cells: could not record the dialog: {e}")),
            None => Ok(()),
        }
    }

    /// Grant `user` the chat verbs on `ai` by name (a wider grant `kern/reach`
    /// wrote is kept), pruning grants of gone cells.
    async fn grant_user(host: &Host, ai: &str) -> Result<(), String> {
        let mut cc: Map<String, Value> = Self::meta_of(host, CHAT_USER)
            .and_then(|m| m.get("can_call").and_then(Value::as_object).cloned())
            .unwrap_or_default();
        cc.retain(|k, _| !k.starts_with("ai_") || k == AI_CELLS_ID || host.kernel.agents.contains_key(&AgentId::from(k.as_str())));
        cc.entry(ai.to_string()).or_insert(json!(CHAT_VERBS));
        let r = host.operator("kernel", json!({"type": "update_agent", "id": CHAT_USER, "can_call": cc})).await.result;
        match r.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("granting `{CHAT_USER}` the cell's chat: {e}")),
            None => Ok(()),
        }
    }

    async fn keep(host: &Host, me: &AgentId, p: &Value) -> Value {
        let ai = p.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(meta) = Self::meta_of(host, &ai) else {
            return refuse("not_found", format!("keep: no AI {ai:?}"), "");
        };
        let mut dialogs = Self::dialogs_map(host, me);
        if dialogs.contains_key(&ai) {
            return json!({"kept": ai, "name": dialogs[&ai]["name"]});
        }
        let from = meta.get("memory_store_id").and_then(Value::as_str).unwrap_or("").to_string();
        let to = match host.memory_store() {
            Ok(m) => m,
            Err(e) => return refuse("not_found", e, "the project has no memory store"),
        };
        // 1 · the rows, RAM → SQLite, under the AI's own key (the operator is
        // both stores' admin)
        let rows = host.operator(&from, json!({"type": "read", "owner": ai})).await.result;
        if let Some(e) = rows.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("keep: reading the cell's rows: {e}"), "");
        }
        let list: Vec<Value> = rows["rows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|mut r| {
                if let Some(o) = r.as_object_mut() {
                    o.remove("owner");
                }
                r
            })
            .collect();
        if !list.is_empty() {
            let r = host.operator(&to, json!({"type": "append", "owner": ai, "rows": list})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return refuse("bad_arg", format!("keep: writing the rows to `{to}`: {e}"), "");
            }
        }
        // 2 · the AI moves onto the SQLite store
        if let Err(e) = host.admit_to_memory(&ai).await {
            return refuse("bad_arg", format!("keep: {e}"), "");
        }
        let mut cc = meta.get("can_call").and_then(Value::as_object).cloned().unwrap_or_default();
        cc.insert(to.clone(), json!("*"));
        let r = host
            .operator("kernel", json!({"type": "update_agent", "id": ai, "memory_store_id": to, "can_call": cc}))
            .await
            .result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("keep: switching the AI to `{to}`: {e}"), "");
        }
        // 3 · the dialog, listed: its name (the id until renamed) and how to
        // re-make its AI
        let mut record: Map<String, Value> =
            meta.iter().filter(|(k, _)| !NOT_SNAPSHOT.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        // the handler is on the record, not in its meta
        let handler = host.kernel.agents.get(&AgentId::from(ai.as_str())).and_then(|a| a.value().record().handler_module);
        record.insert("handler_module".into(), json!(handler));
        let name = p.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(&ai).trim().to_string();
        dialogs.insert(ai.clone(), json!({"name": name, "template": meta.get("template"), "record": record}));
        if let Err(e) = Self::save_dialogs(host, me, dialogs).await {
            return refuse("bad_arg", e, "");
        }
        json!({"kept": ai, "name": name, "rows": rows["rows"].as_array().map_or(0, Vec::len)})
    }

    async fn rename(host: &Host, me: &AgentId, p: &Value) -> Value {
        let ai = p.get("id").and_then(Value::as_str).unwrap_or("");
        let name = p.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let mut dialogs = Self::dialogs_map(host, me);
        let Some(d) = dialogs.get_mut(ai).and_then(Value::as_object_mut) else {
            return refuse("not_found", format!("rename: no kept dialog {ai:?}"), "keep it first");
        };
        d.insert("name".into(), json!(if name.is_empty() { ai.to_string() } else { name.clone() }));
        let shown = d["name"].clone();
        match Self::save_dialogs(host, me, dialogs).await {
            Ok(()) => json!({"id": ai, "name": shown}),
            Err(e) => refuse("bad_arg", e, ""),
        }
    }

    fn dialogs(host: &Host, me: &AgentId) -> Value {
        let d = Self::dialogs_map(host, me);
        json!({"dialogs": d.iter().map(|(id, v)| json!({
            "id": id, "name": v.get("name"), "template": v.get("template"),
            "live": host.kernel.agents.contains_key(&AgentId::from(id.as_str())),
        })).collect::<Vec<_>>()})
    }

    async fn reopen(host: &Host, me: &AgentId, p: &Value) -> Value {
        let ai = p.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let dialogs = Self::dialogs_map(host, me);
        let Some(d) = dialogs.get(&ai) else {
            return refuse("not_found", format!("reopen: no kept dialog {ai:?}"), "");
        };
        let (stamp, peer) = match (stamp_of(p), peer_grants_of(p)) {
            (Ok(s), Ok(g)) => (s, g),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        let leg = Self::leg(host, me);
        // open in another cell already: the SAME agent — one dialog, several cells
        if let Some(meta) = Self::meta_of(host, &ai) {
            let root = host
                .kernel
                .agents
                .get(&AgentId::from(ai.as_str()))
                .and_then(|a| a.value().record().parent_id)
                .filter(|p| *p != leg)
                .unwrap_or_else(|| ai.clone());
            drop(meta);
            return json!({"id": ai, "root": root, "template": d.get("template"), "name": d.get("name"), "shared": true});
        }
        let to = match host.memory_store() {
            Ok(m) => m,
            Err(e) => return refuse("not_found", e, ""),
        };
        if let Err(e) = host.admit_to_memory(&ai).await {
            return refuse("bad_arg", format!("reopen: {e}"), "");
        }
        let mut spec: Map<String, Value> = d.get("record").and_then(Value::as_object).cloned().unwrap_or_default();
        spec.extend(stamp);
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(leg));
        spec.insert("id".into(), json!(ai));
        spec.insert("memory_store_id".into(), json!(to));
        spec.insert("callable_by".into(), json!({ CHAT_USER: CHAT_VERBS }));
        let mut cc = peer;
        cc.insert(to.clone(), json!("*"));
        spec.insert("can_call".into(), Value::Object(cc));
        let handler = spec.get("handler_module").and_then(Value::as_str).unwrap_or("").to_string();
        if let Err(e) = crate::kern::keys::attach(host, &handler, &mut spec) {
            return refuse("bad_arg", format!("reopen: {e}"), "");
        }
        let named = spec.clone();
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("reopen: re-making the AI: {e}"), "");
        }
        if let Err(e) = crate::kern::keys::wire_record(host, &ai, &named).await {
            return refuse("bad_arg", format!("reopen: {e}"), "");
        }
        if let Err(e) = Self::grant_user(host, &ai).await {
            return refuse("bad_arg", e, "");
        }
        json!({"id": ai, "root": ai, "template": d.get("template"), "name": d.get("name"), "shared": false})
    }
}

#[async_trait]
impl Bundle for AiCellsBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "boot" => Ok(None),
            "reflect" => Ok(Some(json!({
                "handler_module": HANDLER_MODULE,
                "sentence": "Spawns the canvas's AI cells from the user's templates: a RAM store and the AI under it, per cell.",
                "verbs": {
                    "presets": "No args. {templates: [{name, handler_module, model}], preferred}.",
                    "spawn": "args: template:str? (else the default), stamp:dict? (the caller's own fields for the backend root, copied unread), peer_grants:dict? (`<guid>|<agent>` keys only — the AI's egress to another kernel). {id, root, template, handler_module, model}.",
                    "keep": "args: id:str (req — the cell's AI), name:str?. Moves the dialog to the project's SQLite memory; it outlives the cell. {kept, name, rows}.",
                    "rename": "args: id:str, name:str. {id, name}.",
                    "dialogs": "No args. The kept dialogs: {dialogs: [{id, name, template, live}]}.",
                    "reopen": "args: id:str (a kept dialog), stamp:dict?, peer_grants:dict? (as spawn). The live AI if another cell has it, else a fresh one on SQLite. {id, root, template, name, shared}.",
                },
            }))),
            _ => {
                let Some(host) = Self::host(kernel) else {
                    return Ok(Some(refuse("not_found", "ai_cells: the kernel has no root", "")));
                };
                Ok(Some(match verb {
                    "presets" => Self::presets(&host).await,
                    "spawn" => Self::spawn(&host, agent_id, payload).await,
                    "keep" => Self::keep(&host, agent_id, payload).await,
                    "rename" => Self::rename(&host, agent_id, payload).await,
                    "dialogs" => Self::dialogs(&host, agent_id),
                    "reopen" => Self::reopen(&host, agent_id, payload).await,
                    _ => refuse("unhandled", format!("ai_cells: unknown type {verb:?}"), ""),
                }))
            }
        }
    }
}
