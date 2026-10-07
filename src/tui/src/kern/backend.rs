//! THE CANVAS'S HOST NODE: `canvas_root` under `canvas_page` — the page records'
//! loader (`web_loader`), the static files, and every backend of the canvas's
//! cells as its OWN direct children (the tree, owner 2026-10-04).
//!
//! ```text
//!   canvas_root                   canvas.tools
//!   ├── exec_bridge_* · setup_* · ai_<hex>   the cells' backends
//!   ├── static_files
//!   └── web_loader
//! ```
//!
//! IT CREATES AND DELETES THE CELLS' BACKENDS: `create {record}` makes one
//! direct child (a cell's backend); `delete {id}` removes an agent of its
//! subtree. The page never writes the host tree itself.
//!
//! AI CELLS. The canvas makes an AI cell wherever you type something that is not
//! a known kind (design: `tmp/canvas_ai.md`). The cell's AI agent is built from
//! one of the user's TEMPLATES, and only the CLI may read those
//! (`kern/templates`), so the page asks `canvas_root`. The AI is its direct
//! child, on the PROJECT's memory store from its first turn (owner D4,
//! 2026-10-03: no per-cell RAM store; a closed, unkept cell's rows stay in
//! SQLite). The store admits it as `user` (the store's parent); its record lets
//! `user` call the chat verbs; `user`'s own side is `kern/reach`'s.
//!
//! Verbs: `presets` (template names, their backend and model, and the default),
//! `spawn {template?, stamp?, peer_grants?}` (the named template, else the
//! owner's default — no default and no name is refused, naming how to set one).
//!
//! The host knows NOTHING of the page's composition. `stamp` is the CALLER's:
//! record fields it wants on the cell's backend (the canvas stamps its cell's
//! id, to reconcile its own pairs), copied without being read. `peer_grants` is
//! more egress for the AI, merged into its `can_call`.
//!
//! KEPT DIALOGS. `keep {id, name?}` records the dialog: a `dialog` row in the
//! memory store under `user`'s key (its name, template and the record to re-make
//! the AI), the newest row per AI winning — `rename` adds one. `dialogs` lists
//! them. `reopen {id, stamp?, peer_grants?}` binds a new cell to the dialog: the
//! live AI when another cell has it open (ONE dialog in several cells, updating
//! live), else the same AI re-made here — its history is in the store under its
//! own id. Which cells show an AI is the PAGE's count: it deletes the AI when its
//! last cell on it closes.

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::env::ClientCfg;
use crate::kern::host::Host;

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "canvas.tools";

/// Who the cell's view calls the AI as: a tokenless tab's user.
const CHAT_USER: &str = crate::env::HUMAN;
/// What the cell's view may call on a spawned AI.
pub const CHAT_VERBS: [&str; 6] = ["send", "history", "status", "watch", "reflect", "stop"];

/// The memory-store row kind a kept dialog is recorded as (under `user`'s key).
const DIALOG: &str = "dialog";

/// A template-record field the dialog snapshot never carries (identity, wiring,
/// authority — added again when the AI is re-made).
const NOT_SNAPSHOT: [&str; 7] = ["id", "parent_id", "guid", "agents", "memory_store_id", "can_call", "callable_by"];

/// Fields a caller's `stamp` may never set: the structural ones (they make a
/// different agent).
const NOT_STAMPABLE: [&str; 6] = ["type", "id", "parent_id", "guid", "handler_module", "agents"];

/// The caller's `stamp`: an object of fields, none of them structural (a
/// structural field makes a different agent). Grants pass: the kernel judges
/// them at the write. Absent = none.
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

/// The caller's `peer_grants`: more egress for the new AI, any grant key (the
/// kernel refuses an illegal one at the write). Absent = none.
fn peer_grants_of(p: &Value) -> Result<Map<String, Value>, Value> {
    match p.get("peer_grants") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err(refuse("bad_arg", "peer_grants: must be an object", "")),
    }
}

/// The bundle.
#[derive(Default)]
pub struct BackendBundle;

fn refuse(reason: &str, error: impl Into<String>, hint: &str) -> Value {
    json!({"error": error.into(), "reason": reason, "hint": hint})
}

pub(crate) fn hex8() -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl BackendBundle {
    fn host(kernel: &Arc<Kernel>) -> Option<Host> {
        let root = kernel.root()?.id.0.clone();
        Some(Host { kernel: Arc::clone(kernel), cfg: ClientCfg::here(), root })
    }

    /// Whether `handler` makes an AI agent: its bundle's record names a memory
    /// store (`memory_store_id`, the field that makes one). Any other template
    /// is a plain HOST agent — a plugin (backend-owned views).
    fn is_ai_handler(host: &Host, handler: &str) -> bool {
        host.kernel
            .bundles
            .catalog()
            .find(|(m, _)| *m == handler)
            .and_then(|(_, b)| b.record_schema())
            .is_some_and(|s| s["fields"].as_array().is_some_and(|a| a.iter().any(|f| f["name"] == "memory_store_id")))
    }

    async fn presets(host: &Host) -> Value {
        match crate::kern::templates::shelf(host).await {
            Ok(sh) => json!({
                "templates": sh.templates.iter().map(|t| json!({
                    "name": t.name, "handler_module": t.handler(), "model": t.model(),
                    // an AI template opens a chat; any other is a plugin: a host
                    // agent drawn by its own view
                    "kind": if Self::is_ai_handler(host, &t.handler()) { "ai" } else { "agent" },
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
        // a STARTER BUNDLE (the double-click's "python agent"): a new project
        // bundle folder from the CLI's starter, loaded, and one instance of it
        if let Some(kind) = p.get("bundle").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Self::spawn_bundle(host, me, kind, stamp).await;
        }
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
        // A PLUGIN template (not an AI): a plain host agent from the record —
        // no memory store, its key by reference, a fork of the template (`from`)
        if !Self::is_ai_handler(host, &t.handler()) {
            return Self::spawn_agent(host, me, &name, t, stamp).await;
        }
        let ai = format!("ai_{}", hex8());
        let mut spec: Map<String, Value> = t.record.clone();
        spec.extend(stamp);
        spec.insert("template".into(), json!(name));
        match Self::make_ai(host, me, &ai, spec, peer).await {
            Ok(()) => json!({"id": ai, "root": ai, "template": name, "handler_module": t.handler(), "model": t.model()}),
            Err(e) => refuse("bad_arg", format!("spawn: the AI from template `{name}`: {e}"), ""),
        }
    }

    /// A starter bundle placed (owner's workflow b): its files copied into a NEW
    /// `.fantastic/bundles/py_<hex>/` (the folder name IS the handler), the kernel
    /// told to load it (`reload_bundle`), one instance made under `canvas_root`,
    /// its view written from the bundle's view file. Editing the folder then takes
    /// effect at once (`kern/bundle_watch`).
    async fn spawn_bundle(host: &Host, me: &AgentId, kind: &str, stamp: Map<String, Value>) -> Value {
        let paths = match crate::env::Paths::here() {
            Ok(p) => p,
            Err(e) => return refuse("not_found", format!("spawn: the project dir: {e}"), ""),
        };
        let name = format!("py_{}", &hex8()[..6]);
        let dir = match crate::kern::bundle_watch::scaffold(&paths.store, kind, &name) {
            Ok(d) => d,
            Err(e) => return refuse("bad_arg", format!("spawn: {e}"), ""),
        };
        let r = host.operator("kernel", json!({"type": "reload_bundle", "name": name})).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("spawn: loading the bundle `{name}`: {e}"), "");
        }
        let id = format!("{name}_1");
        let mut spec = stamp;
        for (k, v) in [("type", json!("create_agent")), ("parent_id", json!(me.0)), ("id", json!(id)), ("handler_module", json!(name))] {
            spec.insert(k.into(), v);
        }
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("spawn: an instance of `{name}`: {e}"), "");
        }
        if let Err(e) = crate::kern::reach::granted(host, &id).await {
            return refuse("bad_arg", e, "");
        }
        if let Err(e) = crate::kern::bundle_watch::refresh_views(host, &name, &dir).await {
            return refuse("bad_arg", format!("spawn: `{id}`'s view: {e}"), "");
        }
        json!({"id": id, "root": id, "bundle": name, "folder": dir.display().to_string(), "kind": "bundle"})
    }

    /// A plugin template placed: a host agent `<name>_<hex8>` under `canvas_root`,
    /// its record the template's (a COPY — `from: <template>`), its key attached
    /// by reference. `kern/reach` grants it to the human and the leg.
    async fn spawn_agent(host: &Host, me: &AgentId, name: &str, t: &crate::kern::templates::Template, stamp: Map<String, Value>) -> Value {
        let slug: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
        let id = format!("{slug}_{}", hex8());
        let mut spec: Map<String, Value> = t.record.clone();
        spec.extend(stamp);
        spec.insert("from".into(), json!(name));
        let handler = t.handler();
        if let Err(e) = crate::kern::keys::attach(host, &handler, &mut spec) {
            return refuse("bad_arg", format!("spawn: `{name}`: {e}"), "");
        }
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(me.0));
        spec.insert("id".into(), json!(id));
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("spawn: the agent from template `{name}`: {e}"), "");
        }
        if let Err(e) = crate::kern::reach::granted(host, &id).await {
            return refuse("bad_arg", e, "");
        }
        json!({"id": id, "root": id, "template": name, "handler_module": handler, "kind": "agent"})
    }

    /// Create AI `ai` as `canvas_root`'s direct child, on the project's memory
    /// store from its first turn (owner D4): the store admits it (as `user`, the
    /// store's parent), its record names the store, `user` may call its chat
    /// verbs, it may call the store, its key and `peer`. `user`'s own side is
    /// `kern/reach`'s.
    pub(crate) async fn make_ai(host: &Host, me: &AgentId, ai: &str, mut spec: Map<String, Value>, peer: Map<String, Value>) -> Result<(), String> {
        // the caller's own store, when it names one (its grants are the
        // caller's); else the project's, which admits the AI
        let store = match spec.get("memory_store_id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => host.admit_to_memory(ai).await?,
        };
        let handler = spec.get("handler_module").and_then(Value::as_str).unwrap_or("").to_string();
        crate::kern::keys::attach(host, &handler, &mut spec)?;
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(me.0));
        spec.insert("id".into(), json!(ai));
        spec.insert("memory_store_id".into(), json!(store));
        // the defaults MERGED with the caller's grants (stamp, peer_grants) —
        // the caller's keys win on a clash; nothing the caller passed is dropped
        let caller = |spec: &mut Map<String, Value>, f: &str| match spec.remove(f) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let (caller_cc, caller_cb) = (caller(&mut spec, "can_call"), caller(&mut spec, "callable_by"));
        let mut cb = Map::new();
        cb.insert(CHAT_USER.into(), json!(CHAT_VERBS));
        cb.extend(caller_cb);
        spec.insert("callable_by".into(), Value::Object(cb));
        let mut cc = Map::new();
        cc.insert(store, json!("*"));
        spec.insert("can_call".into(), Value::Object(cc));
        crate::kern::keys::grant_on_create(&mut spec);
        if let Some(Value::Object(cc)) = spec.get_mut("can_call") {
            cc.extend(peer);
            cc.extend(caller_cc);
        }
        let named = spec.clone();
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(e.to_string());
        }
        if let Err(e) = crate::kern::keys::wire_record(host, ai, &named).await {
            let _ = host.operator("kernel", json!({"type": "delete_agent", "id": ai})).await;
            return Err(e);
        }
        // the cell calls it through `user` and watches it through the leg the
        // moment this answers: both grants first
        crate::kern::reach::granted(host, ai).await
    }

    /// Whether `id` sits under `me` (strictly).
    fn below(host: &Host, me: &AgentId, id: &str) -> bool {
        let mut cur = host.kernel.agents.get(&AgentId::from(id)).and_then(|a| a.value().record().parent_id);
        let mut hops = 0;
        while let Some(p) = cur {
            if p == me.0 {
                return true;
            }
            hops += 1;
            if hops > 4096 {
                return false;
            }
            cur = host.kernel.agents.get(&AgentId::from(p.as_str())).and_then(|a| a.value().record().parent_id);
        }
        false
    }

    /// `create {record}` — one direct child of this backend agent.
    async fn create(host: &Host, me: &AgentId, p: &Value) -> Value {
        let Some(Value::Object(rec)) = p.get("record") else {
            return refuse("bad_arg", "create: record (dict) required", "");
        };
        if let Some(k) = rec.keys().find(|k| *k != "handler_module" && NOT_STAMPABLE.contains(&k.as_str()) && *k != "id") {
            return refuse("bad_arg", format!("create: `{k}` is structural — not the caller's to set"), "the child's parent is this agent");
        }
        let handler = rec.get("handler_module").and_then(Value::as_str).unwrap_or("");
        if handler.is_empty() {
            return refuse("bad_arg", "create: record.handler_module required", "");
        }
        // a child asked again is the same child
        if let Some(id) = rec.get("id").and_then(Value::as_str) {
            if let Some(a) = host.kernel.agents.get(&AgentId::from(id)) {
                let r = a.value().record();
                if r.parent_id.as_deref() == Some(me.0.as_str()) && r.handler_module.as_deref() == Some(handler) {
                    return json!({"id": id, "existed": true});
                }
                return refuse("exists", format!("create: `{id}` already exists elsewhere"), "");
            }
        }
        let mut spec = rec.clone();
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(me.0));
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("create: {e}"), "");
        }
        // the page calls it through `user` and watches it through the leg the
        // moment this answers: both grants first
        let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if let Err(e) = crate::kern::reach::granted(host, &id).await {
            return refuse("not_found", format!("create: {e}"), "");
        }
        r
    }

    /// `delete {id}` — an agent of this node's subtree, and its own subtree.
    async fn delete(host: &Host, me: &AgentId, p: &Value) -> Value {
        let id = p.get("id").and_then(Value::as_str).unwrap_or("");
        if !Self::below(host, me, id) {
            return refuse("not_found", format!("delete: no {id:?} under `{}`", me.0), "");
        }
        host.operator("kernel", json!({"type": "delete_agent", "id": id})).await.result
    }

    /// `canvases` — every canvas, the main one (declarative `main: true`) first.
    fn canvases(host: &Host) -> Value {
        let mut out: Vec<Value> = crate::kern::web::canvases(host)
            .into_iter()
            .map(|(id, loader)| {
                let m = Self::meta_of(host, &id).unwrap_or_default();
                json!({"id": id, "loader": loader, "main": m.get("main") == Some(&json!(true)),
                    "name": m.get("display_name").and_then(Value::as_str).unwrap_or(id.strip_prefix("canvas_").unwrap_or(&id))})
            })
            .collect();
        out.sort_by_key(|c| c["main"] != json!(true));
        json!({"canvases": out})
    }

    fn meta_of(host: &Host, id: &str) -> Option<Map<String, Value>> {
        host.kernel.agents.get(&AgentId::from(id)).map(|a| a.value().record().meta.clone())
    }

    /// The kept dialogs: `dialog` rows in the memory store under `user`'s key
    /// (no handler may write `canvas_root`'s record: grants and fields come
    /// from above), the newest row per AI wins — `{id: {name, template, record}}`.
    async fn dialogs_map(host: &Host) -> Result<Map<String, Value>, String> {
        let store = host.memory_store()?;
        let r = host.act(&store, json!({"type": "read", "kinds": [DIALOG]})).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("backend: reading the kept dialogs: {e}"));
        }
        let mut out = Map::new();
        for row in r["rows"].as_array().cloned().unwrap_or_default() {
            if let Some(ai) = row.get("ai").and_then(Value::as_str) {
                out.insert(ai.to_string(), json!({"name": row.get("name"), "template": row.get("template"), "record": row.get("record")}));
            }
        }
        Ok(out)
    }

    /// One `dialog` row: AI `ai` kept as `name`.
    async fn save_dialog(host: &Host, ai: &str, d: &Value) -> Result<(), String> {
        let store = host.memory_store()?;
        let row = json!({"kind": DIALOG, "ai": ai, "name": d.get("name"), "template": d.get("template"), "record": d.get("record")});
        let r = host.act(&store, json!({"type": "append", "rows": [row]})).await.result;
        match r.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("backend: could not record the dialog: {e}")),
            None => Ok(()),
        }
    }

    /// KEEP: the AI is on the project's memory store from its first turn, so a
    /// kept dialog is a recorded one — its name, template and how to re-make it.
    async fn keep(host: &Host, p: &Value) -> Value {
        let ai = p.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(meta) = Self::meta_of(host, &ai) else {
            return refuse("not_found", format!("keep: no AI {ai:?}"), "");
        };
        let dialogs = match Self::dialogs_map(host).await {
            Ok(d) => d,
            Err(e) => return refuse("not_found", e, ""),
        };
        if let Some(d) = dialogs.get(&ai) {
            return json!({"kept": ai, "name": d["name"]});
        }
        let mut record: Map<String, Value> =
            meta.iter().filter(|(k, _)| !NOT_SNAPSHOT.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        // the handler is on the record, not in its meta
        let handler = host.kernel.agents.get(&AgentId::from(ai.as_str())).and_then(|a| a.value().record().handler_module);
        record.insert("handler_module".into(), json!(handler));
        let name = p.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(&ai).trim().to_string();
        let d = json!({"name": name, "template": meta.get("template"), "record": record});
        match Self::save_dialog(host, &ai, &d).await {
            Ok(()) => json!({"kept": ai, "name": name}),
            Err(e) => refuse("bad_arg", e, ""),
        }
    }

    async fn rename(host: &Host, p: &Value) -> Value {
        let ai = p.get("id").and_then(Value::as_str).unwrap_or("");
        let name = p.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let dialogs = match Self::dialogs_map(host).await {
            Ok(d) => d,
            Err(e) => return refuse("not_found", e, ""),
        };
        let Some(mut d) = dialogs.get(ai).cloned() else {
            return refuse("not_found", format!("rename: no kept dialog {ai:?}"), "keep it first");
        };
        d["name"] = json!(if name.is_empty() { ai.to_string() } else { name });
        match Self::save_dialog(host, ai, &d).await {
            Ok(()) => json!({"id": ai, "name": d["name"]}),
            Err(e) => refuse("bad_arg", e, ""),
        }
    }

    async fn dialogs(host: &Host) -> Value {
        match Self::dialogs_map(host).await {
            Ok(d) => json!({"dialogs": d.iter().map(|(id, v)| json!({
                "id": id, "name": v.get("name"), "template": v.get("template"),
                "live": host.kernel.agents.contains_key(&AgentId::from(id.as_str())),
            })).collect::<Vec<_>>()}),
            Err(e) => refuse("not_found", e, ""),
        }
    }

    /// REOPEN a kept dialog in a new cell: the live AI when another cell has it
    /// open (one dialog, several cells), else the same AI re-made here
    /// — its history is in the memory store under its own id.
    async fn reopen(host: &Host, me: &AgentId, p: &Value) -> Value {
        let ai = p.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let dialogs = match Self::dialogs_map(host).await {
            Ok(d) => d,
            Err(e) => return refuse("not_found", e, ""),
        };
        let Some(d) = dialogs.get(&ai) else {
            return refuse("not_found", format!("reopen: no kept dialog {ai:?}"), "");
        };
        let (stamp, peer) = match (stamp_of(p), peer_grants_of(p)) {
            (Ok(s), Ok(g)) => (s, g),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        if host.kernel.agents.contains_key(&AgentId::from(ai.as_str())) {
            return json!({"id": ai, "root": ai, "template": d.get("template"), "name": d.get("name"), "shared": true});
        }
        let mut spec: Map<String, Value> = d.get("record").and_then(Value::as_object).cloned().unwrap_or_default();
        spec.extend(stamp);
        match Self::make_ai(host, me, &ai, spec, peer).await {
            Ok(()) => json!({"id": ai, "root": ai, "template": d.get("template"), "name": d.get("name"), "shared": false}),
            Err(e) => refuse("bad_arg", format!("reopen: re-making the AI: {e}"), ""),
        }
    }
}

#[async_trait]
impl Bundle for BackendBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    /// HOW TO CALL it — served by the kernel's `docs` verb, never on a reflect.
    fn verb_docs(&self) -> Value {
        json!({
                    "create": "args: record:dict (req — handler_module, id?, its own fields; no parent or grants). One direct child of this agent; asking for an existing child of the same handler again answers it. {id, …}.",
                    "delete": "args: id:str (req — an agent under this one). Deletes it and its subtree.",
                    "presets": "No args. {templates: [{name, handler_module, model}], preferred}.",
                    "spawn": "args: template:str? (else the default), stamp:dict? (the caller's own fields for the backend root, copied unread), peer_grants:dict? (any grant key — more egress for the AI). {id, root, template, handler_module, model}.",
                    "keep": "args: id:str (req — the cell's AI), name:str?. Records the dialog (its rows are already in the project's memory store); it outlives the cell. {kept, name}.",
                    "rename": "args: id:str, name:str. {id, name}.",
                    "dialogs": "No args. The kept dialogs: {dialogs: [{id, name, template, live}]}.",
                    "reopen": "args: id:str (a kept dialog), stamp:dict?, peer_grants:dict? (as spawn). The live AI if another cell has it, else the same AI re-made here, its history intact. {id, root, template, name, shared}.",
                    "new_canvas": "args: name:str (req). A PEER canvas `canvas_<name>` beside this one (flat — never nested): its own page records and page kernel, its own cells' backends. Open it at the page URL with ?canvas=<id>. {id}.",
                    "canvases": "No args. Every canvas, the main one first: {canvases: [{id, loader, main, name}]}.",
                })
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "boot" => Ok(None),
            "reflect" => Ok(Some(json!({
                "handler_module": HANDLER_MODULE,
                "sentence": "The canvas's host node: the cells' HOST backends (exec agents, AIs) are its children — create / delete them, spawn an AI. Cells themselves are placed on the page's view (`<page guid>|view_root`), not here.",
            }))),
            _ => {
                let Some(host) = Self::host(kernel) else {
                    return Ok(Some(refuse("not_found", "canvas: the kernel has no root", "")));
                };
                Ok(Some(match verb {
                    "create" => Self::create(&host, agent_id, payload).await,
                    "delete" => Self::delete(&host, agent_id, payload).await,
                    "presets" => Self::presets(&host).await,
                    "spawn" => Self::spawn(&host, agent_id, payload).await,
                    "keep" => Self::keep(&host, payload).await,
                    "rename" => Self::rename(&host, payload).await,
                    "dialogs" => Self::dialogs(&host).await,
                    "reopen" => Self::reopen(&host, agent_id, payload).await,
                    "new_canvas" => match crate::kern::web::new_canvas(&host, payload.get("name").and_then(Value::as_str).unwrap_or("")).await {
                        Ok(id) => json!({"id": id}),
                        Err(e) => refuse("bad_arg", e, "name it: new_canvas {name: \"research\"}"),
                    },
                    "canvases" => Self::canvases(&host),
                    _ => {
                        let mut r = refuse("unhandled", format!("canvas: unknown type {verb:?}"), "");
                        r["verb"] = json!(verb);
                        r
                    }
                }))
            }
        }
    }
}
