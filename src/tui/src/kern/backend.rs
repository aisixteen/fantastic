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

use fantastic_ai_core::chat_bundle::HANDLER_MODULE as CHAT_HANDLER;
use fantastic_ai_core::remote_provider::PROVIDER_FIELD;

use crate::env::ClientCfg;
use crate::kern::host::Host;

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "canvas.tools";

/// Who the cell's view calls the AI as: a tokenless tab's user.
const CHAT_USER: &str = crate::env::HUMAN;
/// What the cell's view may call on a spawned AI.
pub const CHAT_VERBS: [&str; 6] = ["send", "history", "status", "watch", "reflect", "stop"];

/// What a chat may call on its provider, and the provider admits it for.
const PROVIDER_VERBS: [&str; 3] = ["complete", "watch", "reflect"];

/// The spec fields that are the CHAT's alone (the conversation's, its prompt and its saved
/// set-up's name): the provider's record does not carry them.
const CHAT_ONLY: [&str; 13] = [
    "output_reserve",
    "context_strategy",
    "recent_n",
    "caching",
    "cache_primer_until",
    "transport_retries",
    "misfire_limit",
    "system_prompt",
    "file_bridge_id",
    "system_prompt_file",
    "pre_system_prompt",
    "post_system_prompt",
    "saved",
];

/// The spec fields BOTH records carry: the model the chat runs (the provider's default), and
/// the window it fits the conversation to.
const BOTH: [&str; 2] = ["model", "context_window"];

/// The provider's id of the AI `ai` that [`BackendBundle::make_ai`] makes.
pub(crate) fn provider_id(ai: &str) -> String {
    format!("{ai}_provider")
}

/// The share of `spec` that sits on the CHAT's record: its own fields and the shared ones.
pub(crate) fn chat_part(spec: &Map<String, Value>) -> Map<String, Value> {
    spec.iter().filter(|(k, _)| CHAT_ONLY.contains(&k.as_str()) || BOTH.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// The provider agent the chat `ai` names, when it is an agent here.
pub(crate) fn provider_of(host: &Host, ai: &str) -> Option<String> {
    let p = host.kernel.agents.get(&AgentId::from(ai)).and_then(|a| a.value().record().meta.get(PROVIDER_FIELD).and_then(Value::as_str).map(str::to_string))?;
    host.kernel.agents.contains_key(&AgentId::from(p.as_str())).then_some(p)
}

/// The memory-store row kind a kept dialog is recorded as (under `user`'s key).
const DIALOG: &str = "dialog";

/// A saved set-up-record field the dialog snapshot never carries (identity, wiring,
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
    /// the popup"): every `bundles/<name>/` of the state folder the kernel has loaded —
    /// `{name, sentence}`, sorted — each placeable as a new instance
    /// (`spawn {instance_of}`).
    fn project_bundles(host: &Host) -> Vec<Value> {
        let Ok(state) = host.kernel.state_dir() else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = std::fs::read_dir(state.join("bundles"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("bundle.yaml").is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| host.kernel.bundles.is_folder(n))
            .map(|n| {
                let sentence =
                    std::fs::read_to_string(state.join("bundles").join(&n).join("bundle.yaml"))
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
        out.into_iter().map(|(name, sentence)| json!({"name": name, "sentence": sentence})).collect()
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
        if let Some(kind) = p.get("bundle").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Self::spawn_bundle(host, me, kind, stamp).await;
        }
        // ONE MORE INSTANCE of a bundle the project already holds (the popup's
        // "host · bundles" group)
        if let Some(name) = p.get("instance_of").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Self::spawn_instance(host, me, name, stamp).await;
        }
        let name = match p.get("saved").and_then(Value::as_str).filter(|s| !s.is_empty()) {
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
            return refuse("not_found", format!("no saved set-up `{name}`"), "open ⚙ AI set-up to see your saved set-ups");
        };
        if !matches!(
            t.handler().as_str(),
            "anthropic_backend.tools" | "ollama_backend.tools" | "responses_backend.tools" | "vllm_backend.tools" | "foundation_models_backend.tools"
        ) {
            return refuse("bad_arg", format!("saved set-up `{name}` is not an AI set-up ({})", t.handler()), "open ⚙ AI set-up to see your saved set-ups");
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
            Err(e) => refuse("bad_arg", format!("spawn: the AI from saved set-up `{name}`: {e}"), ""),
        }
    }

    /// A starter bundle placed (owner's workflow b): its files copied into a NEW
    /// state folder's `bundles/py_<hex>/` (the folder name IS the handler), the kernel
    /// told to load it (`reload_bundle`), one instance made under `canvas_root`,
    /// its view written from the bundle's view file. Editing the folder then takes
    /// effect at once (`kern/bundle_watch`).
    async fn spawn_bundle(
        host: &Host,
        me: &AgentId,
        kind: &str,
        stamp: Map<String, Value>,
    ) -> Value {
        let state = match host.kernel.state_dir() {
            Ok(p) => p,
            Err(e) => return refuse("not_found", format!("spawn: the state folder: {e}"), ""),
        };
        let name = format!("py_{}", &hex8()[..6]);
        let dir = match crate::kern::bundle_watch::scaffold(&state, kind, &name) {
            Ok(d) => d,
            Err(e) => return refuse("bad_arg", format!("spawn: {e}"), ""),
        };
        // its files watched BEFORE it loads: an edit from now on reloads it, and
        // the folder's own (late) event does not restart the instance made below
        crate::kern::bundle_watch::arm(host, &name).await;
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

    /// An instance of project bundle `name` (the state folder's `bundles/<name>/`, loaded):
    /// `<name>_<n>` (the next free n) under this canvas node, granted like any
    /// cell's backend, its view written from the bundle's view file.
    async fn spawn_instance(
        host: &Host,
        me: &AgentId,
        name: &str,
        stamp: Map<String, Value>,
    ) -> Value {
        match host.kernel.state_dir() {
            Ok(state) => Self::spawn_instance_in(host, me, name, stamp, &state).await,
            Err(e) => refuse("not_found", format!("spawn: the state folder: {e}"), ""),
        }
    }

    /// [`Self::spawn_instance`] in the state folder `state`.
    async fn spawn_instance_in(
        host: &Host,
        me: &AgentId,
        name: &str,
        stamp: Map<String, Value>,
        state: &std::path::Path,
    ) -> Value {
        let dir = state.join("bundles").join(name);
        if !dir.join("bundle.yaml").is_file() || !host.kernel.bundles.is_folder(name) {
            let have: Vec<String> = Self::project_bundles(host).iter().filter_map(|b| b["name"].as_str().map(str::to_string)).collect();
            return refuse("not_found", format!("spawn: no project bundle `{name}`"), &format!("the project's bundles: {}", have.join(", ")));
        }
        let id = (1..)
            .map(|n| format!("{name}_{n}"))
            .find(|c| !host.kernel.agents.contains_key(&AgentId::from(c.as_str())))
            .expect("an unused instance id");
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
        if let Err(e) = crate::kern::bundle_watch::refresh_views(host, name, &dir).await {
            return refuse("bad_arg", format!("spawn: `{id}`'s view: {e}"), "");
        }
        json!({"id": id, "root": id, "bundle": name, "kind": "bundle"})
    }

    /// Make AI `ai` as `me`'s direct children: a PROVIDER (`<ai>_provider`: the spec's backend
    /// handler, endpoint, models, key, prices) and the CHAT `ai` (`ai_chat.tools`, `provider_id`
    /// naming the provider), both on the project's memory store from the first turn (owner D4).
    /// The store admits both (as `user`, its parent); the chat's record lets `user` call the chat
    /// verbs and names its store and the provider's `complete` / `watch`; the provider admits only
    /// the chat. A spec whose handler is `ai_chat.tools` makes the chat alone, on the provider it
    /// names. `user`'s own side is `kern/reach`'s. A chat refused deletes the provider just made.
    pub(crate) async fn make_ai(host: &Host, me: &AgentId, ai: &str, mut spec: Map<String, Value>, peer: Map<String, Value>) -> Result<(), String> {
        let handler = spec.get("handler_module").and_then(Value::as_str).unwrap_or("").to_string();
        let alone = handler == CHAT_HANDLER;
        let prov = provider_id(ai);
        // the project's store, named or not, admits the AI; a caller's OWN store
        // is the caller's to grant
        let named = spec.get("memory_store_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        let store = match named {
            Some(s) if host.ai_store().ok().as_deref() != Some(s.as_str()) => s,
            _ => {
                let s = host.admit_to_memory(ai).await?;
                if !alone {
                    host.admit_to_memory(&prov).await?;
                }
                s
            }
        };
        if !alone {
            crate::kern::keys::attach(host, &handler, &mut spec)?;
        }
        // a chat alone admitted to a provider here: undone if the chat is then refused
        let mut admitted: Option<String> = None;
        let (provider, chat) = Self::pair_specs(me, ai, &store, spec, peer);
        if let Some(provider) = provider {
            let named = provider.clone();
            let r = host.operator("kernel", Value::Object(provider)).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(e.to_string());
            }
            if let Err(e) = crate::kern::keys::wire_record(host, &prov, &named).await {
                let _ = host.operator("kernel", json!({"type": "delete_agent", "id": prov})).await;
                return Err(e);
            }
        } else if let Some(p) = chat.get(PROVIDER_FIELD).and_then(Value::as_str) {
            admitted = Self::admit_chat(host, p, ai)?.then(|| p.to_string());
        }
        let r = host.operator("kernel", Value::Object(chat)).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            if !alone {
                let _ = host.operator("kernel", json!({"type": "delete_agent", "id": prov})).await;
            }
            if let Some(p) = admitted {
                let _ = host.kernel.edit_callable_by(&AgentId::from(p.as_str()), |m| m.remove(ai).is_some());
            }
            return Err(e.to_string());
        }
        // the cell calls it through `user` and watches it through the leg the
        // moment this answers: both grants first
        if !alone {
            crate::kern::reach::granted(host, &prov).await?;
        }
        crate::kern::reach::granted(host, ai).await
    }

    /// A provider on this kernel admits the chat `ai`: `complete` and `watch`, merged into its
    /// `callable_by`; says whether that added `ai`. An agent here that is not a provider (its
    /// handler is no `*_backend.tools`) is refused by name: a chat is admitted to providers only.
    /// A provider on another kernel (`<guid>|<id>`) is that kernel's to admit.
    fn admit_chat(host: &Host, provider: &str, ai: &str) -> Result<bool, String> {
        let id = AgentId::from(provider);
        let Some(handler) = host.kernel.agents.get(&id).map(|a| a.value().record().handler_module) else {
            return Ok(false);
        };
        if !handler.as_deref().is_some_and(|m| m.ends_with("_backend.tools")) {
            return Err(format!("`{provider}` is not a provider (a `*_backend.tools` agent): `{ai}` is not admitted to it"));
        }
        let mut added = false;
        host.kernel
            .edit_callable_by(&id, |m| {
                added = !m.contains_key(ai);
                m.insert(ai.to_string(), json!(PROVIDER_VERBS));
                true
            })
            .map_err(|e| format!("could not admit `{ai}` to `{provider}`: {e}"))?;
        Ok(added)
    }

    /// The `create_agent` specs (pure) of the PROVIDER of AI `ai` and of the CHAT `ai`, under `me`,
    /// on memory `store`. The provider takes the spec but the chat's own fields ([`CHAT_ONLY`]):
    /// its store grant, its key's `get`, and `callable_by` the chat alone. The chat takes
    /// [`CHAT_ONLY`] and [`BOTH`]: the store's grant and the provider's, the human's chat verbs on
    /// its `callable_by`, and the caller's own grants MERGED over both (the caller's keys win on a
    /// clash). A spec that is already a chat has no provider (`None`): its `provider_id` is its own.
    fn pair_specs(me: &AgentId, ai: &str, store: &str, mut spec: Map<String, Value>, peer: Map<String, Value>) -> (Option<Map<String, Value>>, Map<String, Value>) {
        let taken = |spec: &mut Map<String, Value>, f: &str| match spec.remove(f) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let (caller_cc, caller_cb) = (taken(&mut spec, "can_call"), taken(&mut spec, "callable_by"));
        let alone = spec.get("handler_module").and_then(Value::as_str) == Some(CHAT_HANDLER);
        let prov = provider_id(ai);
        let mut chat = Map::new();
        for (k, v) in &spec {
            if CHAT_ONLY.contains(&k.as_str()) || BOTH.contains(&k.as_str()) || alone {
                chat.insert(k.clone(), v.clone());
            }
        }
        let mut provider = spec;
        provider.retain(|k, _| !CHAT_ONLY.contains(&k.as_str()));
        for (m, id) in [(&mut chat, ai), (&mut provider, prov.as_str())] {
            m.insert("type".into(), json!("create_agent"));
            m.insert("parent_id".into(), json!(me.0));
            m.insert("id".into(), json!(id));
            m.insert("memory_store_id".into(), json!(store));
        }
        chat.insert("handler_module".into(), json!(CHAT_HANDLER));
        let mut cc = Map::new();
        cc.insert(store.to_string(), json!("*"));
        let provider_spec = if alone {
            if let Some(p) = chat.get(PROVIDER_FIELD).and_then(Value::as_str) {
                cc.insert(p.to_string(), json!(PROVIDER_VERBS));
            }
            None
        } else {
            chat.insert(PROVIDER_FIELD.into(), json!(prov));
            cc.insert(prov.clone(), json!(PROVIDER_VERBS));
            provider.insert("callable_by".into(), json!({ai: PROVIDER_VERBS}));
            provider.insert("can_call".into(), json!({store: "*"}));
            crate::kern::keys::grant_on_create(&mut provider);
            Some(provider)
        };
        let mut cb = Map::new();
        cb.insert(CHAT_USER.into(), json!(CHAT_VERBS));
        cb.extend(caller_cb);
        chat.insert("callable_by".into(), Value::Object(cb));
        cc.extend(peer);
        cc.extend(caller_cc);
        chat.insert("can_call".into(), Value::Object(cc));
        (provider_spec, chat)
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
        // an AI is a chat on its provider: asked as either, the chat is the child
        let is_ai = matches!(
            handler,
            "anthropic_backend.tools" | "ollama_backend.tools" | "responses_backend.tools" | "vllm_backend.tools" | "foundation_models_backend.tools" | "ai_chat.tools"
        );
        let made = if is_ai { CHAT_HANDLER } else { handler };
        // a child asked again is the same child
        if let Some(id) = rec.get("id").and_then(Value::as_str) {
            if let Some(a) = host.kernel.agents.get(&AgentId::from(id)) {
                let r = a.value().record();
                if r.parent_id.as_deref() == Some(me.0.as_str()) && r.handler_module.as_deref() == Some(made) {
                    return json!({"id": id, "existed": true});
                }
                return refuse("exists", format!("create: `{id}` already exists elsewhere"), "");
            }
        }
        // an AI is made the way every AI is (`make_ai`): a provider and its chat, on the project's
        // memory store from its first turn, the chat granted its store, its provider and the
        // human's chat verbs. Equipment that is an AI reaches its memory store through this.
        if is_ai {
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
        // the provider `make_ai` made for a chat goes with it
        let provider = Some(provider_id(id)).filter(|p| provider_of(host, id).as_deref() == Some(p.as_str()));
        let r = host.operator("kernel", json!({"type": "delete_agent", "id": id})).await.result;
        if let (Some(p), None) = (provider, r.get("error")) {
            let gone = host.operator("kernel", json!({"type": "delete_agent", "id": p})).await.result;
            if gone.get("error").is_some() {
                return gone;
            }
        }
        r
    }

    /// `add_mcp {url, key?}` — ONE remote MCP server as an agent (`mcp_client`):
    /// the key, when given, stored in the credential store bound to the url's
    /// host; the agent made under this node naming it, and granted its `get`.
    async fn add_mcp(host: &Host, me: &AgentId, p: &Value) -> Value {
        const MCP: &str = fantastic_mcp_client::HANDLER_MODULE;
        let url = p.get("url").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if url.is_empty() {
            return refuse("bad_arg", "add_mcp: url (the server's MCP endpoint) required", "");
        }
        let key = p.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if !key.is_empty() {
            if let Err(e) = crate::kern::keys::store(host, MCP, &url, &key).await {
                return refuse("bad_arg", format!("add_mcp: the key: {e}"), "");
            }
        }
        let host_part = crate::kern::keys::host_of(&url).unwrap_or_default();
        let slug: String = host_part.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
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
                spec.insert("credentials".into(), json!({"store": crate::kern::keys::STORE, "ref": r}));
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
        let id = p.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let Some(meta) = Self::meta_of(host, &id) else {
            return refuse("not_found", format!("configure_mcp: no agent {id:?}"), "");
        };
        if host.kernel.agents.get(&AgentId::from(id.as_str())).and_then(|a| a.value().record().handler_module.clone()).as_deref() != Some(MCP) {
            return refuse("bad_arg", format!("configure_mcp: {id:?} is not an MCP agent"), "");
        }
        let url = p.get("url").and_then(Value::as_str).map(str::trim).filter(|u| !u.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| meta.get("url").and_then(Value::as_str).unwrap_or("").to_string());
        let key = p.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
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
                let mut cc = meta.get("can_call").and_then(Value::as_object).cloned().unwrap_or_default();
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
        host.kernel.agents.get(&AgentId::from(id)).map(|a| a.value().record().meta.clone())
    }

    /// The kept dialogs: `dialog` rows in the AI store under `user`'s key
    /// (no handler may write `canvas_root`'s record: grants and fields come
    /// from above), the newest row per AI wins — `{id: {name, saved set-up, record}}`.
    async fn dialogs_map(host: &Host) -> Result<Map<String, Value>, String> {
        let store = host.ai_store()?;
        let r = host.act(&store, json!({"type": "read", "kinds": [DIALOG]})).await.result;
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
        let r = host.act(&store, json!({"type": "append", "rows": [row]})).await.result;
        match r.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("backend: could not record the dialog: {e}")),
            None => Ok(()),
        }
    }

    /// KEEP: the AI is on the project's memory store from its first turn, so a
    /// kept dialog is a recorded one — its name, saved set-up and how to re-make it.
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
        let snapshot = |m: &Map<String, Value>| -> Map<String, Value> {
            m.iter().filter(|(k, _)| !NOT_SNAPSHOT.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        // the handler is on the record, not in its meta
        let handler_of = |id: &str| host.kernel.agents.get(&AgentId::from(id)).and_then(|a| a.value().record().handler_module);
        // the pair `make_ai` made is ONE provider-shaped record: the provider's fields under the
        // chat's, the provider's handler (a saved set-up's shape); a chat on a provider of its own
        // stays a chat record naming it
        let (mut record, handler) = match Self::meta_of(host, &provider_id(&ai)).filter(|_| provider_of(host, &ai).as_deref() == Some(provider_id(&ai).as_str())) {
            Some(pm) => {
                let mut r = snapshot(&pm);
                r.extend(snapshot(&meta).into_iter().filter(|(k, _)| k != PROVIDER_FIELD));
                (r, handler_of(&provider_id(&ai)))
            }
            None => (snapshot(&meta), handler_of(&ai)),
        };
        record.insert("handler_module".into(), json!(handler));
        let name = p.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(&ai).trim().to_string();
        let d = json!({"name": name, "saved": meta.get("saved"), "record": record});
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
            return json!({"id": ai, "root": ai, "saved": d.get("saved"), "name": d.get("name"), "shared": true});
        }
        let mut spec: Map<String, Value> = d.get("record").and_then(Value::as_object).cloned().unwrap_or_default();
        spec.extend(stamp);
        match Self::make_ai(host, me, &ai, spec, peer).await {
            Ok(()) => json!({"id": ai, "root": ai, "saved": d.get("saved"), "name": d.get("name"), "shared": false}),
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
                    "new_canvas" => match crate::kern::web::new_canvas(&host, &agent_id.0, payload.get("name").and_then(Value::as_str).unwrap_or("")).await {
                        Ok(id) => json!({"id": id}),
                        Err(e) => refuse("bad_arg", e, "name it: new_canvas {name: \"research\"}"),
                    },
                    "canvases" => Self::canvases(&host),
                    "add_mcp" => Self::add_mcp(&host, agent_id, payload).await,
                    "configure_mcp" => Self::configure_mcp(&host, payload).await,
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

/// The canvas's `create` of an AI record, on a real kernel (the memory store the AI names,
/// its grants, and the spec it is made from).
#[cfg(test)]
pub(crate) mod create_tests {
    use super::*;
    use fantastic_kernel::{Agent, Kernel};
    use std::path::PathBuf;

    /// A registered AI handler, registered under the name it carries.
    pub(crate) struct FakeAi(pub(crate) &'static str);

    #[async_trait]
    impl Bundle for FakeAi {
        fn name(&self) -> &str {
            self.0
        }

        async fn handle(&self, _id: &AgentId, _p: &Value, _k: &Arc<Kernel>) -> Result<Option<Value>, BundleError> {
            Ok(None)
        }
    }

    /// A backend whose record is refused at the write: the make of a pair on it fails.
    struct RefusingAi;

    #[async_trait]
    impl Bundle for RefusingAi {
        fn name(&self) -> &str {
            "ollama_backend.tools"
        }

        async fn handle(&self, _id: &AgentId, _p: &Value, _k: &Arc<Kernel>) -> Result<Option<Value>, BundleError> {
            Ok(None)
        }

        fn validate_meta(&self, _meta: &Map<String, Value>, _record: &Map<String, Value>) -> Option<String> {
            Some("a required field is missing".to_string())
        }
    }

    /// A memory store that keeps the rows appended to it and answers a read with all of them.
    #[derive(Default)]
    struct FakeStore(std::sync::Mutex<Vec<Value>>);

    #[async_trait]
    impl Bundle for FakeStore {
        fn name(&self) -> &str {
            "fake_store.tools"
        }

        async fn handle(&self, _id: &AgentId, p: &Value, _k: &Arc<Kernel>) -> Result<Option<Value>, BundleError> {
            let mut rows = self.0.lock().unwrap();
            Ok(Some(match p.get("type").and_then(Value::as_str) {
                Some("append") => {
                    rows.extend(p["rows"].as_array().cloned().unwrap_or_default());
                    json!({"ok": true})
                }
                _ => json!({"rows": *rows}),
            }))
        }
    }

    /// A host on a kernel whose root is `kernel`, the human `user` (its can_call names
    /// the AI the test creates: the reach's grant, already in place), a canvas `canvas_root`
    /// and the memory store `memory`.
    pub(crate) async fn host(name: &str, ai_ids: &[&str]) -> (Host, PathBuf) {
        let dir = std::env::temp_dir().join(format!("fc-create-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
            state_dir: dir.join(".fantastic"),
            work_dir: dir.clone(),
        });
        kernel.bundles.register(
            fantastic_core::HANDLER_MODULE,
            fantastic_core::KernelStateBundle::default(),
        );
        kernel.bundles.register(HANDLER_MODULE, BackendBundle);
        kernel.bundles.register("vllm_backend.tools", FakeAi("vllm_backend.tools"));
        kernel.bundles.register("ai_chat.tools", FakeAi("ai_chat.tools"));
        kernel.bundles.register("ollama_backend.tools", RefusingAi);
        kernel.bundles.register("fake_store.tools", FakeStore::default());
        // `user` is real: the host admits an AI to the store AS `user` (`Host::act`)
        kernel.bundles.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());
        let kernel = Arc::new(kernel);
        let root = Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            // `user` updates records through the kernel (the reach's grant in a real host)
            json!({"callable_by": {crate::env::HUMAN: ["update_agent", "create_agent"]}}).as_object().unwrap().clone(),
            dir.join(".fantastic"),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        let h = Host { kernel, cfg: ClientCfg::here(), root: "kernel".to_string() };
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

    pub(crate) fn record_of(h: &Host, id: &str) -> Map<String, Value> {
        h.kernel.agents.get(&AgentId::from(id)).map(|a| a.value().record().meta.clone()).unwrap_or_default()
    }

    pub(crate) const PV: [&str; 3] = ["complete", "watch", "reflect"];

    pub(crate) fn rec_ai(extra: Value) -> Value {
        let mut r = json!({"handler_module": "vllm_backend.tools", "id": "helper", "memory_store_id": "memory",
            "endpoint": "http://box.example:8000/v1", "model": "m1", "kv_overflow": "evict", "context_strategy": "none"});
        r.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        r
    }

    #[tokio::test]
    async fn an_ai_made_on_a_canvas_is_a_provider_and_a_chat_naming_it() {
        let (h, dir) = host("ai", &["helper", "helper_provider"]).await;
        let me = AgentId::from("canvas_root");
        let out = BackendBundle::create(&h, &me, &json!({"record": rec_ai(json!({}))})).await;
        assert_eq!(out, json!({"id": "helper"}), "{out}");
        for id in ["helper", "helper_provider"] {
            let rec = h.kernel.agents.get(&AgentId::from(id)).unwrap().value().record();
            assert_eq!(rec.parent_id.as_deref(), Some("canvas_root"), "{id} is a child of the canvas");
        }
        let handler = |id: &str| h.kernel.agents.get(&AgentId::from(id)).unwrap().value().record().handler_module;
        assert_eq!(handler("helper").as_deref(), Some("ai_chat.tools"));
        assert_eq!(handler("helper_provider").as_deref(), Some("vllm_backend.tools"));
        let (chat, prov) = (record_of(&h, "helper"), record_of(&h, "helper_provider"));
        // the chat names its provider and holds the conversation; the endpoint is the provider's
        assert_eq!(chat["provider_id"], json!("helper_provider"));
        assert_eq!(chat["memory_store_id"], json!("memory"));
        assert_eq!(chat["model"], json!("m1"));
        assert_eq!(chat["context_strategy"], json!("none"));
        for f in ["endpoint", "credentials", "kv_overflow", "handler_module"] {
            assert!(chat.get(f).is_none(), "the chat holds no `{f}`");
        }
        assert_eq!(prov["endpoint"], json!("http://box.example:8000/v1"));
        assert_eq!(prov["kv_overflow"], json!("evict"));
        assert_eq!(prov["memory_store_id"], json!("memory"));
        assert!(prov.get("provider_id").is_none() && prov.get("context_strategy").is_none(), "the provider holds none of the chat's");
        // both ends of the edge, exactly: the chat calls its store and the provider's complete/watch,
        // the provider admits the chat alone, and the human's chat verbs reach the chat
        // (the kernel adds `$self` to every record's grants)
        let grants = |m: &Map<String, Value>, f: &str| {
            let mut g = m[f].as_object().cloned().unwrap();
            g.remove("$self");
            Value::Object(g)
        };
        assert_eq!(grants(&chat, "can_call"), json!({"memory": "*", "helper_provider": PV}));
        assert_eq!(grants(&chat, "callable_by"), json!({crate::env::HUMAN: CHAT_VERBS}));
        assert_eq!(grants(&prov, "can_call"), json!({"memory": "*"}));
        assert_eq!(grants(&prov, "callable_by"), json!({"helper": PV}));
        // and the store admits both (its ingress side)
        assert_eq!(record_of(&h, "memory")["callable_by"]["helper"], json!("*"), "the store admits the chat");
        assert_eq!(record_of(&h, "memory")["callable_by"]["helper_provider"], json!("*"), "and the provider");
        // asked again as either handler, it is the same child
        let again = BackendBundle::create(&h, &me, &json!({"record": rec_ai(json!({}))})).await;
        assert_eq!(again, json!({"id": "helper", "existed": true}), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chat_refused_deletes_the_provider_just_made() {
        let (h, dir) = host("refused", &["taken", "taken_provider"]).await;
        let me = AgentId::from("canvas_root");
        let first = BackendBundle::make_ai(&h, &me, "taken", rec_ai(json!({})).as_object().unwrap().clone(), Map::new()).await;
        assert!(first.is_ok(), "{first:?}");
        // the chat id is taken now: the second chat is refused, the provider it made first goes
        h.operator("kernel", json!({"type": "delete_agent", "id": "taken_provider"})).await;
        let second = BackendBundle::make_ai(&h, &me, "taken", rec_ai(json!({})).as_object().unwrap().clone(), Map::new()).await;
        assert!(second.is_err(), "the chat `taken` exists");
        assert!(!h.kernel.agents.contains_key(&AgentId::from("taken_provider")), "the provider of the refused chat is gone");
        assert!(h.kernel.agents.contains_key(&AgentId::from("taken")), "the first chat is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_takes_both_and_the_pair_can_be_made_again() {
        let (h, dir) = host("delete", &["helper", "helper_provider", "again", "again_provider"]).await;
        let me = AgentId::from("canvas_root");
        let made = BackendBundle::create(&h, &me, &json!({"record": rec_ai(json!({}))})).await;
        assert_eq!(made, json!({"id": "helper"}), "{made}");
        let out = BackendBundle::delete(&h, &me, &json!({"id": "helper"})).await;
        assert!(out.get("error").is_none(), "{out}");
        for id in ["helper", "helper_provider"] {
            assert!(!h.kernel.agents.contains_key(&AgentId::from(id)), "{id} is gone with the chat");
        }
        // a kept dialog's reopen is `make_ai` on the one record: both agents again
        let again = BackendBundle::make_ai(&h, &me, "again", rec_ai(json!({})).as_object().unwrap().clone(), Map::new()).await;
        assert!(again.is_ok(), "{again:?}");
        assert!(h.kernel.agents.contains_key(&AgentId::from("again_provider")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chat_asked_for_names_its_provider_and_the_provider_admits_it() {
        let (h, dir) = host("chat-only", &["mine"]).await;
        let me = AgentId::from("canvas_root");
        let prov = h
            .operator("kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "prov", "handler_module": "vllm_backend.tools", "callable_by": {"$self": "*"}}))
            .await;
        assert!(prov.result.get("error").is_none(), "{:?}", prov.result);
        let out = BackendBundle::create(
            &h,
            &me,
            &json!({"record": {"handler_module": "ai_chat.tools", "id": "mine", "provider_id": "prov", "memory_store_id": "memory"}}),
        )
        .await;
        assert_eq!(out, json!({"id": "mine"}), "{out}");
        assert!(!h.kernel.agents.contains_key(&AgentId::from("mine_provider")), "no second provider");
        assert_eq!(record_of(&h, "mine")["can_call"]["prov"], json!(PV));
        assert_eq!(record_of(&h, "prov")["callable_by"]["mine"], json!(PV));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chat_alone_is_admitted_to_a_provider_only_and_a_refused_chat_is_not_left_admitted() {
        let (h, dir) = host("admit", &["mine", "other"]).await;
        let me = AgentId::from("canvas_root");
        // a chat naming an agent that is no provider is refused by name, and that agent is untouched
        let r = BackendBundle::create(&h, &me, &json!({"record": {"handler_module": "ai_chat.tools", "id": "mine", "provider_id": "memory", "memory_store_id": "memory"}})).await;
        assert!(r["error"].as_str().is_some_and(|e| e.contains("`memory` is not a provider")), "{r}");
        assert!(!h.kernel.agents.contains_key(&AgentId::from("mine")));
        // a chat refused after the admit leaves the provider as it was
        let prov = h
            .operator("kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "prov", "handler_module": "vllm_backend.tools", "callable_by": {"$self": "*"}}))
            .await;
        assert!(prov.result.get("error").is_none(), "{:?}", prov.result);
        let ok = BackendBundle::create(&h, &me, &json!({"record": {"handler_module": "ai_chat.tools", "id": "other", "provider_id": "prov", "memory_store_id": "memory"}})).await;
        assert_eq!(ok, json!({"id": "other"}), "{ok}");
        let again = BackendBundle::make_ai(&h, &me, "other", json!({"handler_module": "ai_chat.tools", "provider_id": "prov", "memory_store_id": "memory"}).as_object().unwrap().clone(), Map::new()).await;
        assert!(again.is_err(), "the chat `other` exists");
        assert_eq!(record_of(&h, "prov")["callable_by"]["other"], json!(PV), "an admit that was already there stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keep_records_the_pair_as_one_provider_shaped_record() {
        let (h, dir) = host("keep", &["helper", "helper_provider", "memory"]).await;
        let me = AgentId::from("canvas_root");
        let made = BackendBundle::make_ai(&h, &me, "helper", rec_ai(json!({})).as_object().unwrap().clone(), Map::new()).await;
        assert!(made.is_ok(), "{made:?}");
        // the kept dialog is a row on the project's store: that store keeps rows
        h.operator("kernel", json!({"type": "delete_agent", "id": "memory"})).await;
        let store = h
            .operator("kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "memory", "handler_module": "fake_store.tools", "callable_by": {"$self": "*", crate::env::HUMAN: "*"}}))
            .await;
        assert!(store.result.get("error").is_none(), "{:?}", store.result);
        h.kernel.edit_can_call(&AgentId::from(crate::env::HUMAN), |m| m.insert("memory".into(), json!("*")).is_none()).unwrap();
        let out = BackendBundle::keep(&h, &json!({"id": "helper", "name": "kept"})).await;
        assert_eq!(out["kept"], json!("helper"), "{out}");
        let rows = BackendBundle::dialogs_map(&h).await.unwrap();
        let rec = &rows["helper"]["record"];
        assert_eq!(rec["handler_module"], json!("vllm_backend.tools"), "the provider's handler, not the chat's");
        assert_eq!(rec["endpoint"], json!("http://box.example:8000/v1"), "the provider's fields");
        assert_eq!(rec["context_strategy"], json!("none"), "under the chat's");
        assert!(rec.get("provider_id").is_none(), "the saved set-up names no provider: it is made again at reopen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spawn_instance_refuses_a_bad_tui_with_reason_bad_arg() {
        let (h, dir) = host("bad-tui", &["fakeb_1", "fakeb_2"]).await;
        h.kernel.bundles.register_folder("fakeb", Arc::new(FakeAi("vllm_backend.tools")), json!({}));
        let bundle = dir.join(".fantastic").join("bundles").join("fakeb");
        std::fs::create_dir_all(&bundle).unwrap();
        let doc = |show: &str| {
            fantastic_kernel::yaml_io::dump(&json!({"record": {"view": {"tui": [
                {"title": "S", "call": {"verb": "v"}, "show": show}]}}}))
        };
        std::fs::write(bundle.join("bundle.yaml"), doc("grid")).unwrap();
        let state = dir.join(".fantastic");
        let me = AgentId::from("canvas_root");
        let out = BackendBundle::spawn_instance_in(&h, &me, "fakeb", Map::new(), &state).await;
        assert_eq!(out["reason"], "bad_arg", "{out}");
        let e = out["error"].as_str().unwrap();
        assert!(e.contains("view.tui[0].show") && e.contains("grid"), "{e}");
        // a good declaration spawns, and the instance carries it
        std::fs::write(bundle.join("bundle.yaml"), doc("kv")).unwrap();
        let out = BackendBundle::spawn_instance_in(&h, &me, "fakeb", Map::new(), &state).await;
        assert!(out.get("error").is_none(), "{out}");
        let id = out["id"].as_str().unwrap().to_string();
        assert_eq!(record_of(&h, &id)["view"]["tui"][0]["show"], "kv");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_ai_naming_its_callers_own_store_is_not_admitted_by_the_host() {
        let (h, dir) = host("own", &["helper2", "helper2_provider"]).await;
        h.act("kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "own_store", "handler_module": fantastic_core::HANDLER_MODULE}))
            .await;
        let out = BackendBundle::create(
            &h,
            &AgentId::from("canvas_root"),
            &json!({"record": {"handler_module": "vllm_backend.tools", "id": "helper2", "memory_store_id": "own_store"}}),
        )
        .await;
        assert_eq!(out, json!({"id": "helper2"}), "{out}");
        assert_eq!(record_of(&h, "helper2")["memory_store_id"], json!("own_store"));
        assert!(record_of(&h, "own_store").get("callable_by").and_then(|c| c.get("helper2")).is_none(), "a caller's own store is the caller's to grant");
        assert!(record_of(&h, "memory").get("callable_by").and_then(|c| c.get("helper2")).is_none(), "nor is the project's store opened to it");
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
    fn the_pair_specs_split_the_fields_and_merge_the_callers_grants_over_the_defaults() {
        let me = AgentId::from("canvas_root");
        let spec = json!({
            "handler_module": "vllm_backend.tools",
            "endpoint": "http://h/v1",
            "credentials": {"store": "credentials", "ref": "vllm/h"},
            "model": "m1",
            "caching": "auto",
            "can_call": {"tool_a": "*", "memory": ["send"]},
            "callable_by": {"ops": ["send"]},
        });
        let (prov, chat) = BackendBundle::pair_specs(
            &me,
            "helper",
            "memory",
            spec.as_object().unwrap().clone(),
            Map::from_iter([("peer_b".to_string(), json!(["send"]))]),
        );
        let prov = prov.expect("a provider");
        assert_eq!((prov["type"].clone(), prov["parent_id"].clone(), prov["id"].clone()), (json!("create_agent"), json!("canvas_root"), json!("helper_provider")));
        assert_eq!(prov["handler_module"], json!("vllm_backend.tools"));
        assert_eq!(prov["endpoint"], json!("http://h/v1"));
        assert_eq!(prov["model"], json!("m1"));
        assert!(prov.get("caching").is_none() && prov.get("provider_id").is_none());
        // the key's `get` is the provider's; the caller's tool grants are the chat's
        assert_eq!(prov["can_call"], json!({"memory": "*", "credentials": ["get"]}));
        assert_eq!(prov["callable_by"], json!({"helper": ["complete", "watch", "reflect"]}));
        assert_eq!(chat["handler_module"], json!("ai_chat.tools"));
        assert_eq!(chat["id"], json!("helper"));
        assert_eq!(chat["provider_id"], json!("helper_provider"));
        assert_eq!(chat["memory_store_id"], json!("memory"));
        assert_eq!(chat["caching"], json!("auto"));
        assert!(chat.get("endpoint").is_none() && chat.get("credentials").is_none());
        // the caller's own `memory` grant wins a clash; the store is named either way
        assert_eq!(chat["can_call"]["memory"], json!(["send"]));
        assert_eq!(chat["can_call"]["tool_a"], json!("*"));
        assert_eq!(chat["can_call"]["peer_b"], json!(["send"]));
        assert_eq!(chat["can_call"]["helper_provider"], json!(["complete", "watch", "reflect"]));
        assert!(chat["can_call"].get("credentials").is_none());
        // the human's chat verbs and the caller's callers, merged
        assert_eq!(chat["callable_by"][crate::env::HUMAN], json!(CHAT_VERBS));
        assert_eq!(chat["callable_by"]["ops"], json!(["send"]));
    }
}
