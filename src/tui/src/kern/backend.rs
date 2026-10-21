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
//! one of the user's SAVED SET-UPS, and only the CLI may read those
//! (`kern/saved`), so the page asks `canvas_root`. The AI is its direct
//! child, on the PROJECT's memory store from its first turn (owner D4,
//! 2026-10-03: no per-cell RAM store; a closed, unkept cell's rows stay in
//! SQLite). The store admits it as `user` (the store's parent); its record lets
//! `user` call the chat verbs; `user`'s own side is `kern/reach`'s.
//!
//! Verbs: `presets` (the default AI set-up and the project's bundles),
//! `spawn {saved set-up?, stamp?, peer_grants?}` (the named saved set-up, else the
//! owner's default — no default and no name is refused, naming how to set one).
//!
//! The host knows NOTHING of the page's composition. `stamp` is the CALLER's:
//! record fields it wants on the cell's backend (the canvas stamps its cell's
//! id, to reconcile its own pairs), copied without being read. `peer_grants` is
//! more egress for the AI, merged into its `can_call`.
//!
//! KEPT DIALOGS. `keep {id, name?}` records the dialog: a `dialog` row in the
//! memory store under `user`'s key (its name, saved set-up and the record to re-make
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

/// A saved set-up-record field the dialog snapshot never carries (identity, wiring,
/// authority — added again when the AI is re-made).
const NOT_SNAPSHOT: [&str; 7] = [
    "id",
    "parent_id",
    "guid",
    "agents",
    "memory_store_id",
    "can_call",
    "callable_by",
];

/// Fields a caller's `stamp` may never set: the structural ones (they make a
/// different agent).
const NOT_STAMPABLE: [&str; 6] = [
    "type",
    "id",
    "parent_id",
    "guid",
    "handler_module",
    "agents",
];

/// The caller's `stamp`: an object of fields, none of them structural (a
/// structural field makes a different agent). Grants pass: the kernel judges
/// them at the write. Absent = none.
fn stamp_of(p: &Value) -> Result<Map<String, Value>, Value> {
    match p.get("stamp") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => match m.keys().find(|k| NOT_STAMPABLE.contains(&k.as_str())) {
            Some(k) => Err(refuse(
                "bad_arg",
                format!("stamp: `{k}` is not the caller's to set"),
                "stamp only fields of your own",
            )),
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
        Some(Host {
            kernel: Arc::clone(kernel),
            cfg: ClientCfg::here(),
            root,
        })
    }

    /// Whether `handler` makes an AI agent: its bundle's record names a memory
    /// store (`memory_store_id`, the field that makes one). A saved set-up is an AI
    /// set-up; one on any other handler is not offered.
    pub(crate) fn is_ai_handler(host: &Host, handler: &str) -> bool {
        host.kernel
            .bundles
            .catalog()
            .find(|(m, _)| *m == handler)
            .and_then(|(_, b)| b.record_schema())
            .is_some_and(|s| {
                s["fields"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|f| f["name"] == "memory_store_id"))
            })
    }

    async fn presets(host: &Host) -> Value {
        match crate::kern::saved::shelf(host).await {
            Ok(sh) => json!({
                "preferred": sh.preferred,
                "bundles": Self::project_bundles(host),
            }),
            Err(e) => refuse("not_found", e, "the user's saved set-ups are not readable"),
        }
    }

    /// THE PROJECT'S OWN BUNDLES (owner 2026-10-09: "bundle it, then place it from
    /// the popup"): every `.fantastic/bundles/<name>/` the kernel has loaded —
    /// `{name, sentence}`, sorted — each placeable as a new instance
    /// (`spawn {instance_of}`).
    fn project_bundles(host: &Host) -> Vec<Value> {
        let Ok(paths) = crate::env::Paths::here() else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = std::fs::read_dir(paths.store.join("bundles"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("bundle.yaml").is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| host.kernel.bundles.is_folder(n))
            .map(|n| {
                let sentence = std::fs::read_to_string(
                    paths.store.join("bundles").join(&n).join("bundle.yaml"),
                )
                .ok()
                .and_then(|t| serde_yaml::from_str::<Value>(&t).ok())
                .and_then(|d| {
                    d.get("sentence")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
                (n, sentence)
            })
            .collect();
        out.sort();
        out.into_iter()
            .map(|(name, sentence)| json!({"name": name, "sentence": sentence}))
            .collect()
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
        let shelf = match crate::kern::saved::shelf(host).await {
            Ok(s) => s,
            Err(e) => return refuse("not_found", e, "the user's saved set-ups are not readable"),
        };
        // a STARTER BUNDLE (the double-click's "python agent"): a new project
        // bundle folder from the CLI's starter, loaded, and one instance of it
        if let Some(kind) = p
            .get("bundle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Self::spawn_bundle(host, me, kind, stamp).await;
        }
        // ONE MORE INSTANCE of a bundle the project already holds (the popup's
        // "host · bundles" group)
        if let Some(name) = p
            .get("instance_of")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Self::spawn_instance(host, me, name, stamp).await;
        }
        let name = match p
            .get("saved")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(n) => n.to_string(),
            None => match &shelf.preferred {
                Some(n) => n.clone(),
                None => {
                    return refuse(
                        "not_found",
                        "no default AI set-up",
                        "open ⚙ AI set-up: save one and make it your default",
                    )
                }
            },
        };
        let Some(t) = shelf.saved.iter().find(|t| t.name == name) else {
            return refuse(
                "not_found",
                format!("no saved set-up `{name}`"),
                "open ⚙ AI set-up to see your saved set-ups",
            );
        };
        if !Self::is_ai_handler(host, &t.handler()) {
            return refuse(
                "bad_arg",
                format!(
                    "saved set-up `{name}` is not an AI set-up ({})",
                    t.handler()
                ),
                "open ⚙ AI set-up to see your saved set-ups",
            );
        }
        let ai = format!("ai_{}", hex8());
        let mut spec: Map<String, Value> = t.record.clone();
        spec.extend(stamp);
        spec.insert("saved".into(), json!(name));
        match Self::make_ai(host, me, &ai, spec, peer).await {
            // EVERY dialog is KEPT from its start (owner 2026-10-12: no ephemeral
            // chats — the canvas and the terminal keep them alike): listed for reopening
            Ok(()) => {
                let kept = Self::keep(host, &json!({"id": ai})).await;
                json!({"id": ai, "root": ai, "saved": name, "handler_module": t.handler(), "model": t.model(),
                       "dialog_name": kept.get("name").cloned().unwrap_or(json!(ai))})
            }
            Err(e) => refuse(
                "bad_arg",
                format!("spawn: the AI from saved set-up `{name}`: {e}"),
                "",
            ),
        }
    }

    /// A starter bundle placed (owner's workflow b): its files copied into a NEW
    /// `.fantastic/bundles/py_<hex>/` (the folder name IS the handler), the kernel
    /// told to load it (`reload_bundle`), one instance made under `canvas_root`,
    /// its view written from the bundle's view file. Editing the folder then takes
    /// effect at once (`kern/bundle_watch`).
    async fn spawn_bundle(
        host: &Host,
        me: &AgentId,
        kind: &str,
        stamp: Map<String, Value>,
    ) -> Value {
        let paths = match crate::env::Paths::here() {
            Ok(p) => p,
            Err(e) => return refuse("not_found", format!("spawn: the project dir: {e}"), ""),
        };
        let name = format!("py_{}", &hex8()[..6]);
        let dir = match crate::kern::bundle_watch::scaffold(&paths.store, kind, &name) {
            Ok(d) => d,
            Err(e) => return refuse("bad_arg", format!("spawn: {e}"), ""),
        };
        // its files watched BEFORE it loads: an edit from now on reloads it, and
        // the folder's own (late) event does not restart the instance made below
        crate::kern::bundle_watch::arm(host, &name).await;
        let r = host
            .operator("kernel", json!({"type": "reload_bundle", "name": name}))
            .await
            .result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse(
                "bad_arg",
                format!("spawn: loading the bundle `{name}`: {e}"),
                "",
            );
        }
        let id = format!("{name}_1");
        let mut spec = stamp;
        for (k, v) in [
            ("type", json!("create_agent")),
            ("parent_id", json!(me.0)),
            ("id", json!(id)),
            ("handler_module", json!(name)),
        ] {
            spec.insert(k.into(), v);
        }
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse(
                "bad_arg",
                format!("spawn: an instance of `{name}`: {e}"),
                "",
            );
        }
        if let Err(e) = crate::kern::reach::granted(host, &id).await {
            return refuse("bad_arg", e, "");
        }
        if let Err(e) = crate::kern::bundle_watch::refresh_views(host, &name, &dir).await {
            return refuse("bad_arg", format!("spawn: `{id}`'s view: {e}"), "");
        }
        json!({"id": id, "root": id, "bundle": name, "folder": dir.display().to_string(), "kind": "bundle"})
    }

    /// An instance of project bundle `name` (`.fantastic/bundles/<name>/`, loaded):
    /// `<name>_<n>` (the next free n) under this canvas node, granted like any
    /// cell's backend, its view written from the bundle's view file.
    async fn spawn_instance(
        host: &Host,
        me: &AgentId,
        name: &str,
        stamp: Map<String, Value>,
    ) -> Value {
        let paths = match crate::env::Paths::here() {
            Ok(p) => p,
            Err(e) => return refuse("not_found", format!("spawn: the project dir: {e}"), ""),
        };
        let dir = paths.store.join("bundles").join(name);
        if !dir.join("bundle.yaml").is_file() || !host.kernel.bundles.is_folder(name) {
            let have: Vec<String> = Self::project_bundles(host)
                .iter()
                .filter_map(|b| b["name"].as_str().map(str::to_string))
                .collect();
            return refuse(
                "not_found",
                format!("spawn: no project bundle `{name}`"),
                &format!("the project's bundles: {}", have.join(", ")),
            );
        }
        let id = (1..)
            .map(|n| format!("{name}_{n}"))
            .find(|c| !host.kernel.agents.contains_key(&AgentId::from(c.as_str())))
            .expect("an unused instance id");
        let mut spec = stamp;
        for (k, v) in [
            ("type", json!("create_agent")),
            ("parent_id", json!(me.0)),
            ("id", json!(id)),
            ("handler_module", json!(name)),
        ] {
            spec.insert(k.into(), v);
        }
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse(
                "bad_arg",
                format!("spawn: an instance of `{name}`: {e}"),
                "",
            );
        }
        if let Err(e) = crate::kern::reach::granted(host, &id).await {
            return refuse("bad_arg", e, "");
        }
        if let Err(e) = crate::kern::bundle_watch::refresh_views(host, name, &dir).await {
            return refuse("bad_arg", format!("spawn: `{id}`'s view: {e}"), "");
        }
        json!({"id": id, "root": id, "bundle": name, "kind": "bundle"})
    }

    /// Create AI `ai` as `canvas_root`'s direct child, on the project's memory
    /// store from its first turn (owner D4): the store admits it (as `user`, the
    /// store's parent), its record names the store, `user` may call its chat
    /// verbs, it may call the store, its key and `peer`. `user`'s own side is
    /// `kern/reach`'s.
    pub(crate) async fn make_ai(
        host: &Host,
        me: &AgentId,
        ai: &str,
        mut spec: Map<String, Value>,
        peer: Map<String, Value>,
    ) -> Result<(), String> {
        // the project's store, named or not, admits the AI (a cabinet's helper AI
        // names it: equipment is not a reach holder, so nothing else would); a
        // caller's OWN store is the caller's to grant
        let named = spec
            .get("memory_store_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let store = match named {
            Some(s) if host.ai_store().ok().as_deref() != Some(s.as_str()) => s,
            _ => host.admit_to_memory(ai).await?,
        };
        let handler = spec
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        crate::kern::keys::attach(host, &handler, &mut spec)?;
        let spec = Self::ai_create_spec(me, ai, &store, spec, peer);
        let named = spec.clone();
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(e.to_string());
        }
        if let Err(e) = crate::kern::keys::wire_record(host, ai, &named).await {
            let _ = host
                .operator("kernel", json!({"type": "delete_agent", "id": ai}))
                .await;
            return Err(e);
        }
        // the cell calls it through `user` and watches it through the leg the
        // moment this answers: both grants first
        crate::kern::reach::granted(host, ai).await
    }

    /// The `create_agent` spec of AI `ai` under `me`, on memory `store` (pure): the
    /// store's grant on the AI's `can_call`, the human's chat verbs on its `callable_by`,
    /// and the caller's own grants MERGED over both (the caller's keys win on a clash).
    fn ai_create_spec(
        me: &AgentId,
        ai: &str,
        store: &str,
        mut spec: Map<String, Value>,
        peer: Map<String, Value>,
    ) -> Map<String, Value> {
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(me.0));
        spec.insert("id".into(), json!(ai));
        spec.insert("memory_store_id".into(), json!(store));
        let caller = |spec: &mut Map<String, Value>, f: &str| match spec.remove(f) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let (caller_cc, caller_cb) = (
            caller(&mut spec, "can_call"),
            caller(&mut spec, "callable_by"),
        );
        let mut cb = Map::new();
        cb.insert(CHAT_USER.into(), json!(CHAT_VERBS));
        cb.extend(caller_cb);
        spec.insert("callable_by".into(), Value::Object(cb));
        let mut cc = Map::new();
        cc.insert(store.to_string(), json!("*"));
        spec.insert("can_call".into(), Value::Object(cc));
        crate::kern::keys::grant_on_create(&mut spec);
        if let Some(Value::Object(cc)) = spec.get_mut("can_call") {
            cc.extend(peer);
            cc.extend(caller_cc);
        }
        spec
    }

    /// Whether `id` sits under `me` (strictly).
    fn below(host: &Host, me: &AgentId, id: &str) -> bool {
        let mut cur = host
            .kernel
            .agents
            .get(&AgentId::from(id))
            .and_then(|a| a.value().record().parent_id);
        let mut hops = 0;
        while let Some(p) = cur {
            if p == me.0 {
                return true;
            }
            hops += 1;
            if hops > 4096 {
                return false;
            }
            cur = host
                .kernel
                .agents
                .get(&AgentId::from(p.as_str()))
                .and_then(|a| a.value().record().parent_id);
        }
        false
    }

    /// `create {record}` — one direct child of this backend agent.
    async fn create(host: &Host, me: &AgentId, p: &Value) -> Value {
        let Some(Value::Object(rec)) = p.get("record") else {
            return refuse("bad_arg", "create: record (dict) required", "");
        };
        if let Some(k) = rec
            .keys()
            .find(|k| *k != "handler_module" && NOT_STAMPABLE.contains(&k.as_str()) && *k != "id")
        {
            return refuse(
                "bad_arg",
                format!("create: `{k}` is structural — not the caller's to set"),
                "the child's parent is this agent",
            );
        }
        let handler = rec
            .get("handler_module")
            .and_then(Value::as_str)
            .unwrap_or("");
        if handler.is_empty() {
            return refuse("bad_arg", "create: record.handler_module required", "");
        }
        // a child asked again is the same child
        if let Some(id) = rec.get("id").and_then(Value::as_str) {
            if let Some(a) = host.kernel.agents.get(&AgentId::from(id)) {
                let r = a.value().record();
                if r.parent_id.as_deref() == Some(me.0.as_str())
                    && r.handler_module.as_deref() == Some(handler)
                {
                    return json!({"id": id, "existed": true});
                }
                return refuse(
                    "exists",
                    format!("create: `{id}` already exists elsewhere"),
                    "",
                );
            }
        }
        // an AI is made the way every AI is (`make_ai`): on the project's memory store from
        // its first turn, granted its store and the human's chat verbs. Equipment that is
        // an AI reaches its memory store through this, as a canvas's AI always has.
        if Self::is_ai_handler(host, handler) {
            let ai = match rec.get("id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => format!("ai_{}", hex8()),
            };
            return match Self::make_ai(host, me, &ai, rec.clone(), Map::new()).await {
                Ok(()) => json!({"id": ai}),
                Err(e) => refuse("bad_arg", format!("create: {e}"), ""),
            };
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
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Err(e) = crate::kern::reach::granted(host, &id).await {
            return refuse("not_found", format!("create: {e}"), "");
        }
        r
    }

    /// `delete {id}` — an agent of this node's subtree, and its own subtree.
    async fn delete(host: &Host, me: &AgentId, p: &Value) -> Value {
        let id = p.get("id").and_then(Value::as_str).unwrap_or("");
        if !Self::below(host, me, id) {
            return refuse(
                "not_found",
                format!("delete: no {id:?} under `{}`", me.0),
                "",
            );
        }
        host.operator("kernel", json!({"type": "delete_agent", "id": id}))
            .await
            .result
    }

    /// `add_mcp {url, key?}` — ONE remote MCP server as an agent (`mcp_client`):
    /// the key, when given, stored in the credential store bound to the url's
    /// host; the agent made under this node naming it, and granted its `get`.
    async fn add_mcp(host: &Host, me: &AgentId, p: &Value) -> Value {
        const MCP: &str = fantastic_mcp_client::HANDLER_MODULE;
        let url = p
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() {
            return refuse(
                "bad_arg",
                "add_mcp: url (the server's MCP endpoint) required",
                "",
            );
        }
        let key = p
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !key.is_empty() {
            if let Err(e) = crate::kern::keys::store(host, MCP, &url, &key).await {
                return refuse("bad_arg", format!("add_mcp: the key: {e}"), "");
            }
        }
        let host_part = crate::kern::keys::host_of(&url).unwrap_or_default();
        let slug: String = host_part
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let id = format!("mcp_{}_{}", slug.trim_matches('_'), hex8());
        let mut spec = Map::new();
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(me.0));
        spec.insert("id".into(), json!(id));
        spec.insert("handler_module".into(), json!(MCP));
        spec.insert("url".into(), json!(url));
        spec.insert("can_call".into(), json!({}));
        if let Ok(r) = crate::kern::keys::ref_for(MCP, &url) {
            if crate::kern::keys::has(host, &r) {
                spec.insert(
                    "credentials".into(),
                    json!({"store": crate::kern::keys::STORE, "ref": r}),
                );
            }
        }
        crate::kern::keys::grant_on_create(&mut spec);
        // the store admits it BEFORE it exists: `create_agent` boots it, and its
        // boot already lists the server's tools with the key
        if let Err(e) = crate::kern::keys::wire_record(host, &id, &spec).await {
            return refuse("not_found", format!("add_mcp: {e}"), "");
        }
        let r = host.operator("kernel", Value::Object(spec)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("add_mcp: {e}"), "");
        }
        if let Err(e) = crate::kern::reach::granted(host, &id).await {
            return refuse("not_found", format!("add_mcp: {e}"), "");
        }
        json!({"id": id})
    }

    /// `configure_mcp {id, url?, key?}` — an MCP agent's SETTINGS (owner 2026-10-11:
    /// "settings to edit mcp and reset"): a new url and / or key (stored bound to
    /// the url's host, named on the record), then `boot` — it connects again and
    /// lists the server's tools. Neither given: a reset (reconnect as it is).
    async fn configure_mcp(host: &Host, p: &Value) -> Value {
        const MCP: &str = fantastic_mcp_client::HANDLER_MODULE;
        let id = p
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let Some(meta) = Self::meta_of(host, &id) else {
            return refuse("not_found", format!("configure_mcp: no agent {id:?}"), "");
        };
        if host
            .kernel
            .agents
            .get(&AgentId::from(id.as_str()))
            .and_then(|a| a.value().record().handler_module.clone())
            .as_deref()
            != Some(MCP)
        {
            return refuse(
                "bad_arg",
                format!("configure_mcp: {id:?} is not an MCP agent"),
                "",
            );
        }
        let url = p
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                meta.get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
        let key = p
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !key.is_empty() {
            if let Err(e) = crate::kern::keys::store(host, MCP, &url, &key).await {
                return refuse("bad_arg", format!("configure_mcp: the key: {e}"), "");
            }
        }
        let mut upd = json!({"type": "update_agent", "id": id, "url": url});
        // the key for the url's host, when one is stored — named and granted
        if let Ok(r) = crate::kern::keys::ref_for(MCP, &url) {
            if crate::kern::keys::has(host, &r) {
                upd["credentials"] = json!({"store": crate::kern::keys::STORE, "ref": r});
                let mut cc = meta
                    .get("can_call")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                cc.insert(crate::kern::keys::STORE.into(), json!(["get"]));
                upd["can_call"] = Value::Object(cc);
                if let Err(e) = crate::kern::keys::wire(host, &id).await {
                    return refuse("not_found", format!("configure_mcp: {e}"), "");
                }
            }
        }
        let r = host.operator("kernel", upd).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("configure_mcp: {e}"), "");
        }
        let b = host.operator(&id, json!({"type": "boot"})).await.result;
        if let Some(e) = b.get("error").and_then(Value::as_str) {
            return refuse("bad_arg", format!("configure_mcp: reconnecting: {e}"), "");
        }
        json!({"id": id, "url": url})
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
        host.kernel
            .agents
            .get(&AgentId::from(id))
            .map(|a| a.value().record().meta.clone())
    }

    /// The kept dialogs: `dialog` rows in the AI store under `user`'s key
    /// (no handler may write `canvas_root`'s record: grants and fields come
    /// from above), the newest row per AI wins — `{id: {name, saved set-up, record}}`.
    async fn dialogs_map(host: &Host) -> Result<Map<String, Value>, String> {
        let store = host.ai_store()?;
        let r = host
            .act(&store, json!({"type": "read", "kinds": [DIALOG]}))
            .await
            .result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("backend: reading the kept dialogs: {e}"));
        }
        let mut out = Map::new();
        for row in r["rows"].as_array().cloned().unwrap_or_default() {
            if let Some(ai) = row.get("ai").and_then(Value::as_str) {
                out.insert(ai.to_string(), json!({"name": row.get("name"), "saved": row.get("saved"), "record": row.get("record")}));
            }
        }
        Ok(out)
    }

    /// One `dialog` row: AI `ai` kept as `name`.
    async fn save_dialog(host: &Host, ai: &str, d: &Value) -> Result<(), String> {
        let store = host.ai_store()?;
        let row = json!({"kind": DIALOG, "ai": ai, "name": d.get("name"), "saved": d.get("saved"), "record": d.get("record")});
        let r = host
            .act(&store, json!({"type": "append", "rows": [row]}))
            .await
            .result;
        match r.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("backend: could not record the dialog: {e}")),
            None => Ok(()),
        }
    }

    /// KEEP: the AI is on the project's memory store from its first turn, so a
    /// kept dialog is a recorded one — its name, saved set-up and how to re-make it.
    async fn keep(host: &Host, p: &Value) -> Value {
        let ai = p
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
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
        let mut record: Map<String, Value> = meta
            .iter()
            .filter(|(k, _)| !NOT_SNAPSHOT.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // the handler is on the record, not in its meta
        let handler = host
            .kernel
            .agents
            .get(&AgentId::from(ai.as_str()))
            .and_then(|a| a.value().record().handler_module);
        record.insert("handler_module".into(), json!(handler));
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&ai)
            .trim()
            .to_string();
        let d = json!({"name": name, "saved": meta.get("saved"), "record": record});
        match Self::save_dialog(host, &ai, &d).await {
            Ok(()) => json!({"kept": ai, "name": name}),
            Err(e) => refuse("bad_arg", e, ""),
        }
    }

    async fn rename(host: &Host, p: &Value) -> Value {
        let ai = p.get("id").and_then(Value::as_str).unwrap_or("");
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let dialogs = match Self::dialogs_map(host).await {
            Ok(d) => d,
            Err(e) => return refuse("not_found", e, ""),
        };
        let Some(mut d) = dialogs.get(ai).cloned() else {
            return refuse(
                "not_found",
                format!("rename: no kept dialog {ai:?}"),
                "keep it first",
            );
        };
        d["name"] = json!(if name.is_empty() {
            ai.to_string()
        } else {
            name
        });
        match Self::save_dialog(host, ai, &d).await {
            Ok(()) => json!({"id": ai, "name": d["name"]}),
            Err(e) => refuse("bad_arg", e, ""),
        }
    }

    async fn dialogs(host: &Host) -> Value {
        match Self::dialogs_map(host).await {
            Ok(d) => json!({"dialogs": d.iter().map(|(id, v)| json!({
                "id": id, "name": v.get("name"), "saved": v.get("saved"),
                "live": host.kernel.agents.contains_key(&AgentId::from(id.as_str())),
            })).collect::<Vec<_>>()}),
            Err(e) => refuse("not_found", e, ""),
        }
    }

    /// REOPEN a kept dialog in a new cell: the live AI when another cell has it
    /// open (one dialog, several cells), else the same AI re-made here
    /// — its history is in the memory store under its own id.
    async fn reopen(host: &Host, me: &AgentId, p: &Value) -> Value {
        let ai = p
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
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
            return json!({"id": ai, "root": ai, "saved": d.get("saved"), "name": d.get("name"), "shared": true});
        }
        let mut spec: Map<String, Value> = d
            .get("record")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        spec.extend(stamp);
        match Self::make_ai(host, me, &ai, spec, peer).await {
            Ok(()) => {
                json!({"id": ai, "root": ai, "saved": d.get("saved"), "name": d.get("name"), "shared": false})
            }
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
            "create": "args: record:dict (req — handler_module, id?, its own fields; no parent or grants). One direct child of this agent; asking for an existing child of the same handler again answers it. An AI (its handler names a memory store) is made as every AI is: on the project's memory store, granted that store and the human's chat verbs. {id, …}.",
            "delete": "args: id:str (req — an agent under this one). Deletes it and its subtree.",
            "presets": "No args. {preferred (the default AI set-up), bundles: [{name, sentence}] (the project's own bundles)}.",
            "spawn": "args: saved:str? (else the default) | bundle:str (a NEW project bundle from that starter — only when the user asks for a bundle) | instance_of:str (one more instance of a project bundle), stamp:dict? (the caller's own fields for the backend root, copied unread), peer_grants:dict? (any grant key — more egress for the AI). {id, root, saved, handler_module, model}.",
            "keep": "args: id:str (req — the cell's AI), name:str?. Records the dialog (its rows are already in the project's memory store); it outlives the cell. {kept, name}.",
            "rename": "args: id:str, name:str. {id, name}.",
            "dialogs": "No args. The kept dialogs: {dialogs: [{id, name, saved, live}]}.",
            "reopen": "args: id:str (a kept dialog), stamp:dict?, peer_grants:dict? (as spawn). The live AI if another cell has it, else the same AI re-made here, its history intact. {id, root, saved, name, shared}.",
            "new_canvas": "args: name:str (req). A CHILD canvas `canvas_<name>` of this one: its page records nest under this canvas's records, and deleting this canvas deletes it. Its own page kernel and its own cells' backends. Open it at the page URL with ?canvas=<id>. {id}.",
            "canvases": "No args. Every canvas, the main one first: {canvases: [{id, loader, main, name}]}.",
            "configure_mcp": "args: id:str (req — an MCP agent of this canvas), url:str?, key:str? — its settings changed (a new key stored for the url's host and named), then it connects and lists its tools again (`boot`). Neither given: a reset — it reconnects as it is. {id, url}.",
            "add_mcp": "args: url:str (req — the server's MCP endpoint), key:str? (a static API key, sent as Bearer to that host only). One remote MCP server as an agent under this canvas: its tools are its verbs. The key is stored in the credential store (choose where keys are kept first). {id}.",
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "boot" => Ok(None),
            "reflect" => Ok(Some(json!({
                "handler_module": HANDLER_MODULE,
                "sentence": "The canvas's host node: the cells' HOST backends (exec agents, AIs) are its children — create / delete them, spawn an AI. Cells themselves are placed on the page's view (`<page guid>|view_root`), not here.",
            }))),
            _ => {
                let Some(host) = Self::host(kernel) else {
                    return Ok(Some(refuse(
                        "not_found",
                        "canvas: the kernel has no root",
                        "",
                    )));
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
                    "new_canvas" => match crate::kern::web::new_canvas(
                        &host,
                        &agent_id.0,
                        payload.get("name").and_then(Value::as_str).unwrap_or(""),
                    )
                    .await
                    {
                        Ok(id) => json!({"id": id}),
                        Err(e) => refuse("bad_arg", e, "name it: new_canvas {name: \"research\"}"),
                    },
                    "canvases" => Self::canvases(&host),
                    "add_mcp" => Self::add_mcp(&host, agent_id, payload).await,
                    "configure_mcp" => Self::configure_mcp(&host, payload).await,
                    _ => {
                        let mut r =
                            refuse("unhandled", format!("canvas: unknown type {verb:?}"), "");
                        r["verb"] = json!(verb);
                        r
                    }
                }))
            }
        }
    }
}

/// The canvas's `create` of an AI record, on a real kernel (the memory store the AI names,
/// its grants, and the spec it is made from).
#[cfg(test)]
mod create_tests {
    use super::*;
    use fantastic_kernel::{Agent, Kernel};
    use std::path::PathBuf;

    /// A registered AI handler: its bundle's record names a memory store.
    struct FakeAi;

    #[async_trait]
    impl Bundle for FakeAi {
        fn name(&self) -> &str {
            "fake_ai.tools"
        }

        fn record_schema(&self) -> Option<Value> {
            Some(json!({"fields": [{"name": "memory_store_id"}]}))
        }

        async fn handle(
            &self,
            _id: &AgentId,
            _p: &Value,
            _k: &Arc<Kernel>,
        ) -> Result<Option<Value>, BundleError> {
            Ok(None)
        }
    }

    /// A host on a kernel whose root is `kernel`, the human `user` (its can_call names
    /// the AI the test creates: the reach's grant, already in place), a canvas `canvas_root`
    /// and the memory store `memory`.
    async fn host(name: &str, ai_ids: &[&str]) -> (Host, PathBuf) {
        let dir = std::env::temp_dir().join(format!("fc-create-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut kernel = Kernel::new();
        kernel.bundles.register(
            fantastic_core::HANDLER_MODULE,
            fantastic_core::KernelStateBundle::default(),
        );
        kernel.bundles.register(HANDLER_MODULE, BackendBundle);
        kernel.bundles.register("fake_ai.tools", FakeAi);
        // `user` is real: the host admits an AI to the store AS `user` (`Host::act`)
        kernel.bundles.register(
            fantastic_user::HANDLER_MODULE,
            fantastic_user::UserBundle::default(),
        );
        let kernel = Arc::new(kernel);
        let root = Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            // `user` updates records through the kernel (the reach's grant in a real host)
            json!({"callable_by": {crate::env::HUMAN: ["update_agent", "create_agent"]}})
                .as_object()
                .unwrap()
                .clone(),
            dir.join(".fantastic"),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        let h = Host {
            kernel,
            cfg: ClientCfg::here(),
            root: "kernel".to_string(),
        };
        let mut human_cc = Map::new();
        human_cc.insert("kernel".into(), json!(["update_agent", "create_agent"]));
        for ai in ai_ids {
            human_cc.insert(ai.to_string(), json!("*"));
        }
        for spec in [
            json!({"parent_id": "kernel", "id": "memory", "handler_module": fantastic_core::HANDLER_MODULE,
                   "callable_by": {"$self": "*"}}),
            json!({"parent_id": "kernel", "id": crate::env::HUMAN, "handler_module": "user.tools",
                   "can_call": Value::Object(human_cc)}),
            json!({"parent_id": "kernel", "id": "canvas_root", "handler_module": HANDLER_MODULE}),
        ] {
            let mut payload = spec.as_object().unwrap().clone();
            payload.insert("type".into(), json!("create_agent"));
            let out = h.operator("kernel", Value::Object(payload)).await;
            assert!(out.result.get("error").is_none(), "{:?}", out.result);
        }
        (h, dir)
    }

    fn record_of(h: &Host, id: &str) -> Map<String, Value> {
        h.kernel
            .agents
            .get(&AgentId::from(id))
            .map(|a| a.value().record().meta.clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn an_ai_made_on_a_canvas_reaches_the_store_it_names_and_the_store_admits_it() {
        let (h, dir) = host("ai", &["helper"]).await;
        let me = AgentId::from("canvas_root");
        let out = BackendBundle::create(
            &h,
            &me,
            &json!({"record": {"handler_module": "fake_ai.tools", "id": "helper", "memory_store_id": "memory"}}),
        )
        .await;
        assert_eq!(out, json!({"id": "helper"}), "{out}");
        let rec = h
            .kernel
            .agents
            .get(&AgentId::from("helper"))
            .unwrap()
            .value()
            .record();
        assert_eq!(
            rec.parent_id.as_deref(),
            Some("canvas_root"),
            "a child of the canvas"
        );
        let meta = record_of(&h, "helper");
        assert_eq!(meta["memory_store_id"], json!("memory"));
        // the AI reads its store (its own side), and the human's chat verbs reach it
        assert_eq!(meta["can_call"]["memory"], json!("*"));
        assert_eq!(meta["callable_by"][crate::env::HUMAN], json!(CHAT_VERBS));
        // and the store admits it (its ingress side)
        assert_eq!(
            record_of(&h, "memory")["callable_by"]["helper"],
            json!("*"),
            "the store admits the helper"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_ai_naming_its_callers_own_store_is_not_admitted_by_the_host() {
        let (h, dir) = host("own", &["helper2"]).await;
        h.act("kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "own_store", "handler_module": fantastic_core::HANDLER_MODULE}))
            .await;
        let out = BackendBundle::create(
            &h,
            &AgentId::from("canvas_root"),
            &json!({"record": {"handler_module": "fake_ai.tools", "id": "helper2", "memory_store_id": "own_store"}}),
        )
        .await;
        assert_eq!(out, json!({"id": "helper2"}), "{out}");
        assert_eq!(
            record_of(&h, "helper2")["memory_store_id"],
            json!("own_store")
        );
        assert!(
            record_of(&h, "own_store")
                .get("callable_by")
                .and_then(|c| c.get("helper2"))
                .is_none(),
            "a caller's own store is the caller's to grant"
        );
        assert!(
            record_of(&h, "memory")
                .get("callable_by")
                .and_then(|c| c.get("helper2"))
                .is_none(),
            "nor is the project's store opened to it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_plain_agent_made_on_a_canvas_names_no_store() {
        let (h, dir) = host("plain", &["plain_exec"]).await;
        let out = BackendBundle::create(
            &h,
            &AgentId::from("canvas_root"),
            &json!({"record": {"handler_module": "file_bridge.tools", "id": "plain_exec"}}),
        )
        .await;
        assert_eq!(out.get("id"), Some(&json!("plain_exec")), "{out}");
        let meta = record_of(&h, "plain_exec");
        assert!(meta.get("memory_store_id").is_none());
        assert!(meta.get("can_call").and_then(|c| c.get("memory")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ai_create_spec_grants_its_store_and_merges_the_callers_grants_over_the_defaults() {
        let me = AgentId::from("canvas_root");
        let spec = json!({
            "handler_module": "fake_ai.tools",
            "can_call": {"tool_a": "*", "memory": ["send"]},
            "callable_by": {"ops": ["send"]},
        });
        let spec = BackendBundle::ai_create_spec(
            &me,
            "helper",
            "memory",
            spec.as_object().unwrap().clone(),
            Map::from_iter([("peer_b".to_string(), json!(["send"]))]),
        );
        assert_eq!(spec["type"], json!("create_agent"));
        assert_eq!(spec["parent_id"], json!("canvas_root"));
        assert_eq!(spec["id"], json!("helper"));
        assert_eq!(spec["memory_store_id"], json!("memory"));
        // the caller's own `memory` grant wins a clash; the store is named either way
        assert_eq!(spec["can_call"]["memory"], json!(["send"]));
        assert_eq!(spec["can_call"]["tool_a"], json!("*"));
        assert_eq!(spec["can_call"]["peer_b"], json!(["send"]));
        // the human's chat verbs and the caller's callers, merged
        assert_eq!(spec["callable_by"][crate::env::HUMAN], json!(CHAT_VERBS));
        assert_eq!(spec["callable_by"]["ops"], json!(["send"]));
        assert!(spec.get("can_call").and_then(|c| c.as_object()).is_some());
    }
}
