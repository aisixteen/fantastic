//! THE CANVAS'S HOST NODE: `canvas_root` under `user` — THE USER SPACE: the page
//! records' loader (`web_loader`), the garage, and every backend of the canvas's
//! cells as its OWN direct children (the tree, owner 2026-10-04; v9 2026-11-23).
//!
//! ```text
//!   canvas_root                   canvas.tools
//!   ├── exec_bridge_* · setup_* · ai_<hex>   the cells' backends
//!   ├── canvas_garage
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
//! child, on the PROJECT's memory store from its first pass (owner D4,
//! 2026-10-03: no per-cell RAM store; a closed, unkept cell's rows stay in
//! SQLite). Its record holds its own lines (`$parent` `send`, the store, the system
//! block, its provider), handed out by the canvas node from its own lines; the person
//! reaches it by position (it is in the user space).
//!
//! Its verbs are declared in [`verbs`]; `reflect` and `docs` render them.
//!
//! The host knows NOTHING of the page's composition. `stamp` is the CALLER's:
//! record fields it wants on the cell's backend (the canvas stamps its cell's
//! id, to reconcile its own pairs), copied without being read. `peer_grants` is
//! more lines for the AI, merged into its `can`.
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
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use fantastic_ai_core::chat_bundle::HANDLER_MODULE as CHAT_HANDLER;
use fantastic_ai_core::remote_provider::PROVIDER_FIELD;

use crate::env::ClientCfg;

/// What [`BackendBundle::pair_specs`] makes of a spec: the provider's spec (none for a spec
/// that is already a chat), the chat's, and the lines the asker supplied.
type PairSpecs = (Option<Map<String, Value>>, Map<String, Value>, Map<String, Value>);
use crate::kern::cps::{self, Done};
use crate::kern::host::Host;

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "canvas.tools";

pub mod record;
pub mod verbs;

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "The canvas's host node: the cells' host backends (exec agents, AIs) are its children, made and deleted through it. Cells themselves are placed on the page's view, not here.";

/// What a chat may call on its provider: its line on it.
const PROVIDER_VERBS: [&str; 3] = ["complete", "watch", "capabilities"];

/// The spec fields BOTH records carry: the model the chat runs (the provider's default), and
/// the window it fits the conversation to.
const BOTH: [&str; 2] = ["model", "context_window"];

/// Who a canvas verb's kernel calls (`create_agent`, `update_agent`, `delete_agent`) go
/// as. The shell's own calls (the root AI, the saved set-ups) are the operator's. A verb
/// that acts for an asker is checked as the asker, rule 5 included (gm rule 7, Q39):
///
/// - its CREATES are the canvas node's own, checked as the asker too
///   ([`create_child`]): the asker's lines must allow the child as asked, and the canvas
///   node sends the create as itself, so its own create line writes the child (its forced
///   values, the garage's forced `can` among them) and its own lines are what it hands
///   out. The child is what both allow;
/// - its deletes and updates go as the asker ([`Host::asker_then`]).
///
/// An asker's flow is a chain of continuations (#795), and a continuation runs with no
/// identity of its own: [`By::here`] CAPTURES the identity of the verb's dispatch (its
/// asker, and the agent's own sender and origin), and each call of the chain is made
/// under it.
#[derive(Debug, Clone)]
pub(crate) enum By {
    /// The in-process shell: [`Host::operator_then`].
    Operator,
    /// The asker of the running dispatch, inside the canvas node's handler.
    Asker { asker: Option<fantastic_kernel::Asker>, agent: fantastic_kernel::Inherited },
}

impl By {
    /// The identity of the dispatch running here: its asker and the agent's own.
    pub(crate) fn here() -> Self {
        By::Asker { asker: fantastic_kernel::Asker::current(), agent: fantastic_kernel::Inherited::capture() }
    }

    fn is_asker(&self) -> bool {
        matches!(self, By::Asker { .. })
    }

    /// `work` under the running agent's own identity (nothing for the operator).
    async fn within<R>(&self, work: impl std::future::Future<Output = R>) -> R {
        match self {
            By::Operator => work.await,
            By::Asker { agent, .. } => agent.clone().scope(work).await,
        }
    }

    /// THE AGENT'S OWN CALL (the operator's for the shell): `then` runs once, with the settle
    /// or the launch's refusal.
    pub(crate) async fn own<F, Fut>(&self, host: &Host, target: &str, payload: Value, then: F)
    where
        F: FnOnce(fantastic_kernel::Outcome) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        match self {
            By::Operator => host.operator_then(target, payload, then).await,
            By::Asker { agent, .. } => agent.clone().scope(host.agent_then(target, payload, then)).await,
        }
    }

    /// THE ASKER'S CALL (the operator's for the shell).
    pub(crate) async fn asker<F, Fut>(&self, host: &Host, target: &str, payload: Value, then: F)
    where
        F: FnOnce(fantastic_kernel::Outcome) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        match self {
            By::Operator => host.operator_then(target, payload, then).await,
            By::Asker { asker, .. } => host.asker_then(asker, target, payload, then).await,
        }
    }
}

/// Send `payload` to the root as `by`; `then` gets the reply. For [`By::Asker`]: as the asker.
async fn to_kernel<F, Fut>(host: &Host, by: &By, payload: Value, then: F)
where
    F: FnOnce(Value) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    by.asker(host, "kernel", payload, move |out| then(out.result)).await;
}

/// THE CANVAS'S CREATE of one child, `spec` a `create_agent` payload naming its
/// parent. For [`By::Asker`] (rule 7: "also checked as the asker"):
///
/// 1. the asker's check ([`Kernel::check_asker_create`]) on the child as asked: its
///    bundle, id and fields, and `asked` — the `can` lines the ASKER supplied (`None`:
///    none). The lines the canvas adds itself (the store, the provider, `$parent`, the
///    key) are the canvas node's own hand-outs;
/// 2. the id it answers and the values the asker's lines force go into the spec;
/// 3. the canvas node sends the create AS ITSELF ([`By::own`]): its create line and its
///    cap must allow the child, force their values over it, and hold every line of its
///    `can` (the asker's included).
///
/// For [`By::Operator`]: the operator's create. `then` gets the reply.
async fn create_child<F, Fut>(host: &Host, by: &By, mut spec: Map<String, Value>, asked: Option<Value>, then: F)
where
    F: FnOnce(Value) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    if !by.is_asker() {
        return by.own(host, "kernel", Value::Object(spec), move |out| then(out.result)).await;
    }
    let parent = AgentId::from(spec.get("parent_id").and_then(Value::as_str).unwrap_or(""));
    let mut record: Map<String, Value> = spec.iter().filter(|(k, _)| !matches!(k.as_str(), "type" | "parent_id" | "can")).map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(can) = asked.filter(|c| c.as_object().is_none_or(|m| !m.is_empty())) {
        record.insert("can".into(), can);
    }
    // the check reads the identity of the running dispatch: the captured one
    let checked = by.within(async { host.kernel.check_asker_create(&parent, &record) }).await;
    match checked {
        Err(refused) => then(refused).await,
        Ok(ok) => {
            spec.insert("id".into(), json!(ok.id));
            spec.extend(ok.forced);
            by.own(host, "kernel", Value::Object(spec), move |out| then(out.result)).await;
        }
    }
}

/// Undo a create [`create_child`] made, as its creator: the operator, or the canvas
/// node itself for [`By::Asker`] (it made the child). `then` gets the reply.
async fn undo_create<F, Fut>(host: &Host, by: &By, id: &str, then: F)
where
    F: FnOnce(Value) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    by.own(host, "kernel", json!({"type": "delete_agent", "id": id}), move |out| then(out.result)).await;
}

/// The provider's id of the AI `ai` that [`BackendBundle::make_ai`] makes.
pub(crate) fn provider_id(ai: &str) -> String {
    format!("{ai}_provider")
}

/// A spec field the provider's record does not carry: one the CHAT's record declares (its
/// conversation's settings, its prompt, its request knobs, #632) but [`BOTH`].
fn chat_only(k: &str) -> bool {
    !BOTH.contains(&k) && fantastic_ai_core::chat_bundle::RECORD.schema()["properties"].get(k).is_some()
}

/// The share of `spec` that sits on the CHAT's record: its own fields, its knobs and the
/// shared ones.
pub(crate) fn chat_part(spec: &Map<String, Value>) -> Map<String, Value> {
    spec.iter().filter(|(k, _)| chat_only(k) || BOTH.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// ONE AI spec split onto the pair's two records (#632): the PROVIDER's share (every field
/// the chat's record does not own) and the CHAT's ([`chat_part`]). The one split
/// [`BackendBundle::pair_specs`] makes and a saved set-up is judged by
/// (`kern::saved::save`).
pub(crate) fn split(spec: &Map<String, Value>) -> (Map<String, Value>, Map<String, Value>) {
    let provider = spec
        .iter()
        .filter(|(k, _)| !chat_only(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (provider, chat_part(spec))
}

/// The provider agent the chat `ai` names, when it is an agent here.
pub(crate) fn provider_of(host: &Host, ai: &str) -> Option<String> {
    let p = host.kernel.resolve_target(ai).and_then(|a| a.record().meta.get(PROVIDER_FIELD).and_then(Value::as_str).map(str::to_string))?;
    host.kernel.exists(p.as_str()).then_some(p)
}

/// The memory-store row kind a kept dialog is recorded as (under `user`'s key).
const DIALOG: &str = "dialog";

/// A saved set-up-record field the dialog snapshot never carries (identity, wiring,
/// authority — added again when the AI is re-made).
const NOT_SNAPSHOT: [&str; 6] = ["id", "parent_id", "guid", "agents", "memory_store_id", "can"];

/// What an AI's line on the memory store calls (the tree's chat line): its rows, and
/// `copy` of its own rows to a fork it makes.
const STORE_VERBS: [&str; 3] = ["append", "read", "copy"];

/// What a chat calls on its parent, the canvas it sits on (`initial-tree.md`, "What a
/// new chat cell gets"). The canvas hands it out from its own self line (Q50).
const PARENT_VERBS: [&str; 1] = ["send"];

/// What a chat calls on the project's system block (the `prompt_file` view): the
/// verbs the lib's prompt loader sends it — its text (`read`) and its changes (`watch`).
const PROMPT_VERBS: [&str; 2] = ["read", "watch"];

/// The bundle.
#[derive(Default)]
pub struct BackendBundle;

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

    async fn presets(host: &Host, done: Done<Value>) {
        let h = host.clone();
        crate::kern::saved::shelf(
            host,
            cps::done(move |r: Result<crate::kern::saved::Shelf, String>| async move {
                done(match r {
                    Ok(sh) => json!({
                        "preferred": sh.preferred,
                        "bundles": Self::project_bundles(&h),
                    }),
                    Err(e) => fantastic_kernel::refusal_hint(fantastic_kernel::refusal::NOT_FOUND, e, "the user's saved set-ups are not readable"),
                })
                .await;
            }),
        )
        .await;
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
            .filter_map(|n| {
                let sentence =
                    std::fs::read_to_string(state.join("bundles").join(&n).join("bundle.yaml"))
                        .ok()
                        .and_then(|t| serde_yaml::from_str::<Value>(&t).ok())
                        .and_then(|d| {
                            d.get("sentence")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })?;
                Some((n, sentence))
            })
            .collect();
        out.sort();
        out.into_iter().map(|(name, sentence)| json!({"name": name, "sentence": sentence})).collect()
    }

    async fn spawn(host: &Host, by: &By, me: &AgentId, a: verbs::Spawn, done: Done<Value>) {
        let (host2, by2, me2) = (host.clone(), by.clone(), me.clone());
        crate::kern::saved::shelf(
            host,
            cps::done(move |r: Result<crate::kern::saved::Shelf, String>| async move {
                let (host, by, me) = (host2, by2, me2);
                let stamp = a.stamp;
                let peer = a.peer_grants;
                let shelf = match r {
                    Ok(s) => s,
                    Err(e) => return done(fantastic_kernel::refusal_hint(fantastic_kernel::refusal::NOT_FOUND, e, "the user's saved set-ups are not readable")).await,
                };
                // a STARTER BUNDLE (the double-click's "python agent"): a new project
                // bundle folder from the CLI's starter, loaded, and one instance of it
                if let Some(kind) = a.bundle.as_deref() {
                    return Self::spawn_bundle(&host, &by, &me, kind, stamp, done).await;
                }
                // ONE MORE INSTANCE of a bundle the project already holds (the popup's
                // "host · bundles" group)
                if let Some(name) = a.instance_of.as_deref() {
                    return Self::spawn_instance(&host, &by, &me, name, stamp, done).await;
                }
                let name = match a.saved {
                    Some(n) => n,
                    None => match &shelf.preferred {
                        Some(n) => n.clone(),
                        None => {
                            return done(fantastic_kernel::refusal_hint(fantastic_kernel::refusal::NOT_FOUND, "no default AI set-up", "open ⚙ AI set-up: save one and make it your default")).await;
                        }
                    },
                };
                let Some(t) = shelf.saved.iter().find(|t| t.name == name) else {
                    return done(fantastic_kernel::refusal_hint(fantastic_kernel::refusal::NOT_FOUND, format!("no saved set-up `{name}`"), "open ⚙ AI set-up to see your saved set-ups")).await;
                };
                if let Err(r) = crate::kern::setup::check_backend(&host, &t.handler()) {
                    return done(fantastic_kernel::refusal_hint(fantastic_kernel::refusal::BAD_ARG, format!("saved set-up `{name}` is not an AI set-up: {}", r["error"].as_str().unwrap_or("")), "open ⚙ AI set-up to see your saved set-ups")).await;
                }
                let ai = format!("ai_{}", hex8());
                let mut spec: Map<String, Value> = t.record.clone();
                spec.extend(stamp);
                // what the set-up is stays what it is, whatever the stamp says
                spec.insert("handler_module".into(), json!(t.handler()));
                let (handler, model) = (t.handler(), t.model());
                let (h2, by2, ai2, name2) = (host.clone(), by.clone(), ai.clone(), name.clone());
                Self::make_ai(
                    &host,
                    &by,
                    &me,
                    &ai,
                    spec,
                    peer,
                    cps::done(move |r: Result<(), String>| async move {
                        match r {
                            // EVERY dialog is KEPT from its start (owner 2026-10-12: no ephemeral
                            // chats — the canvas and the terminal keep them alike): listed for reopening
                            Ok(()) => {
                                let ai3 = ai2.clone();
                                Self::keep_with(
                                    &h2,
                                    &by2,
                                    ai2,
                                    None,
                                    Some(name2.clone()),
                                    cps::done(move |kept: Value| async move {
                                        if kept.get("error").is_some() {
                                            return done(kept).await;
                                        }
                                        done(json!({"id": ai3, "root": ai3, "saved": name2, "handler_module": handler, "model": model,
                                               "dialog_name": kept["name"]}))
                                        .await;
                                    }),
                                )
                                .await;
                            }
                            Err(e) => done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("spawn: the AI from saved set-up `{name2}`: {e}"))).await,
                        }
                    }),
                )
                .await;
            }),
        )
        .await;
    }

    /// A starter bundle placed (owner's workflow b): its files copied into a NEW
    /// state folder's `bundles/py_<hex>/` (the folder name IS the handler), the kernel
    /// told to load it (`reload_bundle`), one instance made under `canvas_root`,
    /// its view written from the bundle's view file. Editing the folder then takes
    /// effect at once (`kern/bundle_watch`).
    async fn spawn_bundle(
        host: &Host,
        by: &By,
        me: &AgentId,
        kind: &str,
        stamp: Map<String, Value>,
        done: Done<Value>,
    ) {
        // BUNDLES ARE MERGED BY THE OWNER (rule 12): the bundle is loaded by the asker's
        // own `reload_bundle` on the root, so an asker that may not load one is refused
        // before any file is written
        if let Some(asker) = fantastic_kernel::current_origin() {
            if !host.kernel.may_call(&asker, "reload_bundle", &AgentId::from(host.root.as_str())) {
                return done(fantastic_kernel::unknown_agent(&host.root)).await;
            }
        }
        let state = match host.kernel.state_dir() {
            Ok(p) => p,
            Err(e) => return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, format!("spawn: the state folder: {e}"))).await,
        };
        let name = format!("py_{}", &hex8()[..6]);
        let dir = match crate::kern::bundle_watch::scaffold(&state, kind, &name) {
            Ok(d) => d,
            Err(e) => return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("spawn: {e}"))).await,
        };
        // its files watched BEFORE it loads: an edit from now on reloads it, and
        // the folder's own (late) event does not restart the instance made below
        let (h, by2, me, name2) = (host.clone(), by.clone(), me.clone(), name.clone());
        crate::kern::bundle_watch::arm(
            host,
            &name,
            cps::done(move |()| async move {
                let (h2, by3, me2, name3) = (h.clone(), by2.clone(), me.clone(), name2.clone());
                to_kernel(&h, &by2, json!({"type": "reload_bundle", "name": name2}), move |r| async move {
                    if let Some(e) = r.get("error").and_then(Value::as_str) {
                        return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("spawn: loading the bundle `{name3}`: {e}"))).await;
                    }
                    let id = format!("{name3}_1");
                    let mut spec = stamp;
                    for (k, v) in [("type", json!("create_agent")), ("parent_id", json!(me2.0)), ("id", json!(id)), ("handler_module", json!(name3))] {
                        spec.insert(k.into(), v);
                    }
                    let asked = spec.get("can").cloned();
                    let (h3, name4, id2) = (h2.clone(), name3.clone(), id.clone());
                    create_child(&h2, &by3, spec, asked, move |r| async move {
                        if let Some(e) = r.get("error").and_then(Value::as_str) {
                            return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("spawn: an instance of `{name4}`: {e}"))).await;
                        }
                        let folder = dir.display().to_string();
                        let (name5, id3, dir2) = (name4.clone(), id2.clone(), dir.clone());
                        crate::kern::bundle_watch::refresh_views(
                            &h3,
                            &name4,
                            &dir2,
                            cps::done(move |r: Result<(), String>| async move {
                                if let Err(e) = r {
                                    return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("spawn: `{id3}`'s view: {e}"))).await;
                                }
                                done(json!({"id": id2, "root": id2, "bundle": name5, "folder": folder, "kind": "bundle"})).await;
                            }),
                        )
                        .await;
                    })
                    .await;
                })
                .await;
            }),
        )
        .await;
    }

    /// An instance of project bundle `name` (the state folder's `bundles/<name>/`, loaded):
    /// `<name>_<n>` (the next free n) under this canvas node, granted like any
    /// cell's backend, its view written from the bundle's view file.
    async fn spawn_instance(host: &Host, by: &By, me: &AgentId, name: &str, stamp: Map<String, Value>, done: Done<Value>) {
        match host.kernel.state_dir() {
            Ok(state) => Self::spawn_instance_in(host, by, me, name, stamp, &state, done).await,
            Err(e) => done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, format!("spawn: the state folder: {e}"))).await,
        }
    }

    /// [`Self::spawn_instance`] in the state folder `state`: the create, then — when it
    /// settled — the view.
    async fn spawn_instance_in(host: &Host, by: &By, me: &AgentId, name: &str, stamp: Map<String, Value>, state: &std::path::Path, done: Done<Value>) {
        let dir = state.join("bundles").join(name);
        if !dir.join("bundle.yaml").is_file() || !host.kernel.bundles.is_folder(name) {
            let have: Vec<String> = Self::project_bundles(host).iter().filter_map(|b| b["name"].as_str().map(str::to_string)).collect();
            return done(fantastic_kernel::refusal_hint(fantastic_kernel::refusal::NOT_FOUND, format!("spawn: no project bundle `{name}`"), format!("the project's bundles: {}", have.join(", ")))).await;
        }
        let id = (1..)
            .map(|n| format!("{name}_{n}"))
            .find(|c| !host.kernel.exists(c.as_str()))
            .expect("an unused instance id");
        let mut spec = stamp;
        for (k, v) in [("type", json!("create_agent")), ("parent_id", json!(me.0)), ("id", json!(id)), ("handler_module", json!(name))] {
            spec.insert(k.into(), v);
        }
        let asked = spec.get("can").cloned();
        let (h, name) = (host.clone(), name.to_string());
        create_child(host, by, spec, asked, move |r| async move {
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("spawn: an instance of `{name}`: {e}"))).await;
            }
            let (name2, id2) = (name.clone(), id.clone());
            crate::kern::bundle_watch::refresh_views(
                &h,
                &name,
                &dir,
                cps::done(move |r: Result<(), String>| async move {
                    if let Err(e) = r {
                        return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("spawn: `{id2}`'s view: {e}"))).await;
                    }
                    done(json!({"id": id, "root": id, "bundle": name2, "kind": "bundle"})).await;
                }),
            )
            .await;
        })
        .await;
    }

    /// Make AI `ai` as `me`'s direct children: a PROVIDER (`<ai>_provider`: the spec's backend
    /// handler, endpoint, models, key, prices) and the CHAT `ai` (`ai_chat.tools`, `provider_id`
    /// naming the provider), both on the project's memory store from the first pass (owner D4).
    /// The chat's record holds its lines on the store and on the provider's `complete` / `watch`;
    /// the provider's its line on the store (and its key's `get`). A spec whose handler is
    /// `ai_chat.tools` makes the chat alone, on the provider it names (#632: the caller names the
    /// provider; a new one is made only when none is named). A chat refused deletes the provider
    /// just made. Its creates go as `by` ([`create_child`]: for an asker, checked as the asker
    /// with the lines the asker supplied, then made by the canvas node `me`), and that delete
    /// as their creator.
    pub(crate) async fn make_ai(host: &Host, by: &By, me: &AgentId, ai: &str, mut spec: Map<String, Value>, peer: Map<String, Value>, done: Done<Result<(), String>>) {
        let handler = spec.get("handler_module").and_then(Value::as_str).unwrap_or("").to_string();
        let alone = handler == CHAT_HANDLER;
        let prov = provider_id(ai);
        // the store the spec names, else the project's
        let named = spec.get("memory_store_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        let store = match named {
            Some(s) => s,
            None => match host.ai_store() {
                Ok(s) => s,
                Err(e) => return done(Err(e)).await,
            },
        };
        if !alone {
            if let Err(e) = crate::kern::keys::attach(host, &handler, &mut spec) {
                return done(Err(e)).await;
            }
        }
        let prompt = host.kernel.exists(crate::kern::prompt::BRIDGE).then_some(crate::kern::prompt::BRIDGE);
        let (provider, chat, asked) = Self::pair_specs(me, ai, &store, prompt, spec, peer);
        if provider.is_none() {
            if let Some(p) = chat.get(PROVIDER_FIELD).and_then(Value::as_str) {
                if let Err(e) = Self::is_provider(host, p, ai) {
                    return done(Err(e)).await;
                }
            }
        }
        // the chat is made when the provider's create settled; a chat refused deletes the provider
        let (h, by2) = (host.clone(), by.clone());
        let make_chat = move |done: Done<Result<(), String>>| async move {
            let (h2, by3) = (h.clone(), by2.clone());
            create_child(&h, &by2, chat, Some(Value::Object(asked)), move |r| async move {
                let Some(e) = r.get("error").and_then(Value::as_str).map(str::to_string) else {
                    return done(Ok(())).await;
                };
                if alone {
                    return done(Err(e)).await;
                }
                undo_create(&h2, &by3, &prov, move |_| async move { done(Err(e)).await }).await;
            })
            .await;
        };
        match provider {
            Some(provider) => {
                create_child(host, by, provider, None, move |r| async move {
                    match r.get("error").and_then(Value::as_str) {
                        Some(e) => done(Err(e.to_string())).await,
                        None => make_chat(done).await,
                    }
                })
                .await;
            }
            None => make_chat(done).await,
        }
    }

    /// A chat names a provider: an agent here that is not one (its handler is no
    /// `*_backend.tools`) is refused by name. A provider on another kernel (`<guid>|<id>`), or
    /// one not here, is not judged.
    fn is_provider(host: &Host, provider: &str, ai: &str) -> Result<(), String> {
        let Some(handler) = host.kernel.resolve_target(provider).map(|a| a.record().handler_module) else {
            return Ok(());
        };
        if !handler.as_deref().is_some_and(|m| m.ends_with("_backend.tools")) {
            return Err(format!("`{provider}` is not a provider (a `*_backend.tools` agent): `{ai}` cannot name it"));
        }
        Ok(())
    }

    /// The `create_agent` specs (pure) of the PROVIDER of AI `ai` and of the CHAT `ai`, under `me`,
    /// on memory `store`. The provider takes the spec but the chat's own fields (`chat_only`:
    /// those the chat's record declares, its request knobs among them): its line on the store and
    /// its key's `get`. The chat takes those and [`BOTH`]: the lines a new chat cell gets
    /// (`initial-tree.md`) — `$parent` `send`, the store (`copy` included, for its forks), the
    /// system block `prompt` when that view is here, and its provider's verbs — and the caller's
    /// own lines MERGED over them (the caller's win on a clash). A spec that is already a chat has
    /// no provider (`None`): its `provider_id` is its own. The third part is the lines the ASKER
    /// supplied (the peer's and the caller's): the asker's check holds the asker to them.
    fn pair_specs(me: &AgentId, ai: &str, store: &str, prompt: Option<&str>, mut spec: Map<String, Value>, peer: Map<String, Value>) -> PairSpecs {
        let taken = |spec: &mut Map<String, Value>, f: &str| match spec.remove(f) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let caller_can = taken(&mut spec, "can");
        let alone = spec.get("handler_module").and_then(Value::as_str) == Some(CHAT_HANDLER);
        let prov = provider_id(ai);
        let (mut provider, mut chat) = split(&spec);
        if alone {
            chat = spec;
        }
        for (m, id) in [(&mut chat, ai), (&mut provider, prov.as_str())] {
            m.insert("type".into(), json!("create_agent"));
            m.insert("parent_id".into(), json!(me.0));
            m.insert("id".into(), json!(id));
            m.insert("memory_store_id".into(), json!(store));
        }
        chat.insert("handler_module".into(), json!(CHAT_HANDLER));
        // the chat's own lines: the store, its provider's verbs, the peer's and the
        // caller's (the caller's win on a clash); the provider's: the store and,
        // when its record names a key here, `get` on the key store
        let mut can = Map::new();
        can.insert("$parent".to_string(), json!({"call": PARENT_VERBS}));
        can.insert(store.to_string(), json!({"call": STORE_VERBS}));
        if let Some(p) = prompt {
            can.insert(p.to_string(), json!({"call": PROMPT_VERBS}));
        }
        let provider_spec = if alone {
            if let Some(p) = chat.get(PROVIDER_FIELD).and_then(Value::as_str) {
                can.insert(p.to_string(), json!({"call": PROVIDER_VERBS}));
            }
            None
        } else {
            chat.insert(PROVIDER_FIELD.into(), json!(prov));
            can.insert(prov.clone(), json!({"call": PROVIDER_VERBS}));
            provider.insert("can".into(), json!({store: {"call": STORE_VERBS}}));
            crate::kern::keys::grant_on_create(&mut provider);
            Some(provider)
        };
        let mut asked = peer;
        asked.extend(caller_can);
        can.extend(asked.clone());
        chat.insert("can".into(), Value::Object(can));
        (provider_spec, chat, asked)
    }

    /// Whether `id` sits under `me` (strictly).
    fn below(host: &Host, me: &AgentId, id: &str) -> bool {
        let mut cur = host.kernel.resolve_target(id).and_then(|a| a.record().parent_id);
        let mut hops = 0;
        while let Some(p) = cur {
            if p == me.0 {
                return true;
            }
            hops += 1;
            if hops > 4096 {
                return false;
            }
            cur = host.kernel.resolve_target(p.as_str()).and_then(|a| a.record().parent_id);
        }
        false
    }

    /// `create {record}` — one direct child of this backend agent.
    async fn create(host: &Host, by: &By, me: &AgentId, child: verbs::ChildRecord, done: Done<Value>) {
        let handler = child.handler_module.clone();
        let handler = handler.as_str();
        let want_id = child.id.clone();
        let rec = child.into_map();
        // an AI is a chat on its provider: asked as either, the chat is the child
        let is_ai = crate::kern::setup::check_backend(host, handler).is_ok() || handler == CHAT_HANDLER;
        let made = if is_ai { CHAT_HANDLER } else { handler };
        // a child asked again is the same child
        if let Some(id) = want_id.as_deref() {
            if let Some(a) = host.kernel.resolve_target(id) {
                let r = a.record();
                if r.parent_id.as_deref() == Some(me.0.as_str()) && r.handler_module.as_deref() == Some(made) {
                    return done(json!({"id": id, "existed": true})).await;
                }
                return done(fantastic_kernel::refusal(fantastic_kernel::refusal::EXISTS, format!("create: `{id}` already exists elsewhere"))).await;
            }
        }
        // an AI is made the way every AI is (`make_ai`): a provider and its chat, on the project's
        // memory store from its first pass, the chat holding its lines on its store and its
        // provider. Equipment that is an AI reaches its memory store through this.
        if is_ai {
            let ai = match want_id {
                Some(id) => id,
                None => format!("ai_{}", hex8()),
            };
            let ai2 = ai.clone();
            return Self::make_ai(
                host,
                by,
                me,
                &ai,
                rec,
                Map::new(),
                cps::done(move |r: Result<(), String>| async move {
                    done(match r {
                        Ok(()) => json!({"id": ai2}),
                        Err(e) => fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("create: {e}")),
                    })
                    .await;
                }),
            )
            .await;
        }
        let mut spec = rec;
        spec.insert("type".into(), json!("create_agent"));
        spec.insert("parent_id".into(), json!(me.0));
        let asked = spec.get("can").cloned();
        create_child(host, by, spec, asked, move |r| async move {
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("create: {e}"))).await;
            }
            done(r).await;
        })
        .await;
    }

    /// `delete {id}` — an agent of this node's subtree, and its own subtree. A provider is an
    /// agent like any other (#632): it stays when a chat on it goes, until it is deleted itself.
    async fn delete(host: &Host, by: &By, me: &AgentId, id: &str, done: Done<Value>) {
        if !Self::below(host, me, id) {
            return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, format!("delete: no {id:?} under `{}`", me.0))).await;
        }
        to_kernel(host, by, json!({"type": "delete_agent", "id": id}), move |r| async move { done(r).await }).await;
    }

    /// `add_mcp {url, key?}` — ONE remote MCP server as an agent (`mcp_client`):
    /// the key, when given, stored in the credential store bound to the url's
    /// host; the agent made under this node naming it, holding its line `get` on the store.
    async fn add_mcp(host: &Host, by: &By, me: &AgentId, a: verbs::AddMcp, done: Done<Value>) {
        const MCP: &str = fantastic_mcp_client::HANDLER_MODULE;
        let url = a.url;
        let stored_url = url.clone();
        let (h, by2, me2) = (host.clone(), by.clone(), me.clone());
        let after_key = move |r: Result<(), String>| async move {
            if let Err(e) = r {
                return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("add_mcp: the key: {e}"))).await;
            }
            let host_part = crate::kern::keys::host_of(&url).unwrap_or_default();
            let slug: String = host_part.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let id = format!("mcp_{}_{}", slug.trim_matches('_'), hex8());
            let mut spec = Map::new();
            spec.insert("type".into(), json!("create_agent"));
            spec.insert("parent_id".into(), json!(me2.0));
            spec.insert("id".into(), json!(id));
            spec.insert("handler_module".into(), json!(MCP));
            spec.insert("url".into(), json!(url));
            spec.insert("can".into(), json!({}));
            if let Ok(r) = crate::kern::keys::ref_for(MCP, &url) {
                if crate::kern::keys::has(&h, &r) {
                    spec.insert("credentials".into(), json!({"store": crate::kern::keys::STORE, "ref": r}));
                }
            }
            // its line on the store is in its record: `create_agent` boots it, and its
            // boot already lists the server's tools with the key
            crate::kern::keys::grant_on_create(&mut spec);
            create_child(&h, &by2, spec, None, move |r| async move {
                if let Some(e) = r.get("error").and_then(Value::as_str) {
                    return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("add_mcp: {e}"))).await;
                }
                done(json!({"id": id})).await;
            })
            .await;
        };
        match a.key {
            Some(key) => {
                crate::kern::keys::store(host, MCP, &stored_url, &key, cps::done(move |r: Result<String, String>| async move { after_key(r.map(|_| ())).await })).await;
            }
            None => after_key(Ok(())).await,
        }
    }

    /// `configure_mcp {id, url?, key?}` — an MCP agent's SETTINGS (owner 2026-10-11:
    /// "settings to edit mcp and reset"): a new url and / or key (stored bound to
    /// the url's host, named on the record), then `boot` — it connects again and
    /// lists the server's tools. Neither given: a reset (reconnect as it is).
    async fn configure_mcp(host: &Host, by: &By, a: verbs::ConfigureMcp, done: Done<Value>) {
        const MCP: &str = fantastic_mcp_client::HANDLER_MODULE;
        let id = a.id;
        let Some(meta) = Self::meta_of(host, &id) else {
            return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, format!("configure_mcp: no agent {id:?}"))).await;
        };
        if host.kernel.resolve_target(id.as_str()).and_then(|a| a.record().handler_module.clone()).as_deref() != Some(MCP) {
            return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("configure_mcp: {id:?} is not an MCP agent"))).await;
        }
        let url = match a.url {
            Some(u) => u,
            None => match meta.get("url").and_then(Value::as_str) {
                Some(u) => u.to_string(),
                None => return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("configure_mcp: {id:?} records no url"))).await,
            },
        };
        let stored_url = url.clone();
        let (h, by2) = (host.clone(), by.clone());
        let after_key = move |r: Result<(), String>| async move {
            if let Err(e) = r {
                return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("configure_mcp: the key: {e}"))).await;
            }
            let mut upd = json!({"type": "update_agent", "id": id, "url": url});
            // the key for the url's host, when one is stored — named, with its line `get`
            if let Ok(r) = crate::kern::keys::ref_for(MCP, &url) {
                if crate::kern::keys::has(&h, &r) {
                    upd["credentials"] = json!({"store": crate::kern::keys::STORE, "ref": r});
                    let mut can = meta.get("can").and_then(Value::as_object).cloned().unwrap_or_default();
                    can.insert(crate::kern::keys::STORE.into(), json!({"call": ["get"]}));
                    upd["can"] = Value::Object(can);
                }
            }
            let (h2, by3) = (h.clone(), by2.clone());
            to_kernel(&h, &by2, upd, move |r| async move {
                if let Some(e) = r.get("error").and_then(Value::as_str) {
                    return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("configure_mcp: {e}"))).await;
                }
                // the reconnect is the asker's too: it updated the agent, it boots it
                by3.asker(&h2, &id.clone(), json!({"type": "boot"}), move |b| async move {
                    if let Some(e) = b.result.get("error").and_then(Value::as_str) {
                        return done(fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("configure_mcp: reconnecting: {e}"))).await;
                    }
                    done(json!({"id": id, "url": url})).await;
                })
                .await;
            })
            .await;
        };
        match a.key {
            Some(key) => {
                crate::kern::keys::store(host, MCP, &stored_url, &key, cps::done(move |r: Result<String, String>| async move { after_key(r.map(|_| ())).await })).await;
            }
            None => after_key(Ok(())).await,
        }
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
        host.kernel.resolve_target(id).map(|a| a.record().meta.clone())
    }

    /// The kept dialogs: `dialog` rows in the AI store, the newest row per AI wins — `{id: {name,
    /// saved set-up, record}}`. Read with [`By::own`]: inside a handler (the canvas
    /// node's, a setup agent's) the read is that agent's, checked against its own line on the
    /// store, and the rows are those under its own key; from the shell, the operator's. `done`
    /// gets them with the read's settle.
    pub(crate) async fn dialogs_map(host: &Host, by: &By, done: Done<Result<Map<String, Value>, String>>) {
        let store = match host.ai_store() {
            Ok(s) => s,
            Err(e) => return done(Err(e)).await,
        };
        by.own(host, &store, json!({"type": "read", "kinds": [DIALOG]}), move |out| async move {
            let r = out.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return done(Err(format!("backend: reading the kept dialogs: {e}"))).await;
            }
            let mut out = Map::new();
            for row in r["rows"].as_array().cloned().unwrap_or_default() {
                if let Some(ai) = row.get("ai").and_then(Value::as_str) {
                    out.insert(ai.to_string(), json!({"name": row.get("name"), "saved": row.get("saved"), "record": row.get("record")}));
                }
            }
            done(Ok(out)).await;
        })
        .await;
    }

    /// One `dialog` row: AI `ai` kept as `name`. Appended with [`By::own`], as
    /// [`Self::dialogs_map`] reads: under the key of the agent whose handler runs it.
    async fn save_dialog(host: &Host, by: &By, ai: &str, d: &Value, done: Done<Result<(), String>>) {
        let store = match host.ai_store() {
            Ok(s) => s,
            Err(e) => return done(Err(e)).await,
        };
        let row = json!({"kind": DIALOG, "ai": ai, "name": d.get("name"), "saved": d.get("saved"), "record": d.get("record")});
        by.own(host, &store, json!({"type": "append", "rows": [row]}), move |out| async move {
            done(match out.result.get("error").and_then(Value::as_str) {
                Some(e) => Err(format!("backend: could not record the dialog: {e}")),
                None => Ok(()),
            })
            .await;
        })
        .await;
    }

    /// KEEP: the AI is on the project's memory store from its first pass, so a
    /// kept dialog is a recorded one — its name, saved set-up and how to re-make it.
    async fn keep(host: &Host, by: &By, a: verbs::Keep, done: Done<Value>) {
        Self::keep_with(host, by, a.id, a.name, None, done).await;
    }

    /// `keep`, with the saved set-up the AI was made from (`spawn` hands it over;
    /// the dialog row is the only place it is written).
    async fn keep_with(host: &Host, by: &By, ai: String, name: Option<String>, saved: Option<String>, done: Done<Value>) {
        let Some(meta) = Self::meta_of(host, &ai) else {
            return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, format!("keep: no AI {ai:?}"))).await;
        };
        let (h, by2) = (host.clone(), by.clone());
        Self::dialogs_map(
            host,
            by,
            cps::done(move |r: Result<Map<String, Value>, String>| async move {
                let host = h;
                let dialogs = match r {
                    Ok(d) => d,
                    Err(e) => return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, e)).await,
                };
                if let Some(d) = dialogs.get(&ai) {
                    return done(json!({"kept": ai, "name": d["name"]})).await;
                }
                let snapshot = |m: &Map<String, Value>| -> Map<String, Value> {
                    m.iter().filter(|(k, _)| !NOT_SNAPSHOT.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
                };
                // the handler is on the record, not in its meta
                let handler_of = |id: &str| host.kernel.resolve_target(id).and_then(|a| a.record().handler_module);
                // the pair `make_ai` made is ONE provider-shaped record: the provider's fields under the
                // chat's, the provider's handler (a saved set-up's shape); a chat on a provider of its own
                // stays a chat record naming it
                let (mut record, handler) = match Self::meta_of(&host, &provider_id(&ai)).filter(|_| provider_of(&host, &ai).as_deref() == Some(provider_id(&ai).as_str())) {
                    Some(pm) => {
                        let mut r = snapshot(&pm);
                        r.extend(snapshot(&meta).into_iter().filter(|(k, _)| k != PROVIDER_FIELD));
                        (r, handler_of(&provider_id(&ai)))
                    }
                    None => (snapshot(&meta), handler_of(&ai)),
                };
                record.insert("handler_module".into(), json!(handler));
                let name = name.unwrap_or_else(|| ai.clone());
                let d = json!({"name": name, "saved": saved, "record": record});
                Self::save_dialog(
                    &host,
                    &by2,
                    &ai.clone(),
                    &d,
                    cps::done(move |r: Result<(), String>| async move {
                        done(match r {
                            Ok(()) => json!({"kept": ai, "name": name}),
                            Err(e) => fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, e),
                        })
                        .await;
                    }),
                )
                .await;
            }),
        )
        .await;
    }

    async fn rename(host: &Host, by: &By, a: verbs::Rename, done: Done<Value>) {
        let ai = a.id;
        let name = a.name;
        let (h, by2) = (host.clone(), by.clone());
        Self::dialogs_map(
            host,
            by,
            cps::done(move |r: Result<Map<String, Value>, String>| async move {
                let dialogs = match r {
                    Ok(d) => d,
                    Err(e) => return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, e)).await,
                };
                let Some(mut d) = dialogs.get(&ai).cloned() else {
                    return done(fantastic_kernel::refusal_hint(fantastic_kernel::refusal::NOT_FOUND, format!("rename: no kept dialog {ai:?}"), "keep it first")).await;
                };
                d["name"] = json!(name);
                let named = d["name"].clone();
                Self::save_dialog(
                    &h,
                    &by2,
                    &ai.clone(),
                    &d,
                    cps::done(move |r: Result<(), String>| async move {
                        done(match r {
                            Ok(()) => json!({"id": ai, "name": named}),
                            Err(e) => fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, e),
                        })
                        .await;
                    }),
                )
                .await;
            }),
        )
        .await;
    }

    async fn dialogs(host: &Host, by: &By, done: Done<Value>) {
        let h = host.clone();
        Self::dialogs_map(
            host,
            by,
            cps::done(move |r: Result<Map<String, Value>, String>| async move {
                done(match r {
                    Ok(d) => json!({"dialogs": d.iter().map(|(id, v)| json!({
                        "id": id, "name": v.get("name"), "saved": v.get("saved"),
                        "live": h.kernel.exists(id.as_str()),
                    })).collect::<Vec<_>>()}),
                    Err(e) => fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, e),
                })
                .await;
            }),
        )
        .await;
    }

    /// REOPEN a kept dialog in a new cell: the live AI when another cell has it
    /// open (one dialog, several cells), else the same AI re-made here
    /// — its history is in the memory store under its own id.
    async fn reopen(host: &Host, by: &By, me: &AgentId, a: verbs::Reopen, done: Done<Value>) {
        let ai = a.id;
        let (h, by2, me2) = (host.clone(), by.clone(), me.clone());
        Self::dialogs_map(
            host,
            by,
            cps::done(move |r: Result<Map<String, Value>, String>| async move {
                let dialogs = match r {
                    Ok(d) => d,
                    Err(e) => return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, e)).await,
                };
                let Some(d) = dialogs.get(&ai) else {
                    return done(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, format!("reopen: no kept dialog {ai:?}"))).await;
                };
                let stamp = a.stamp;
                let peer = a.peer_grants;
                let (saved, named) = (d.get("saved").cloned(), d.get("name").cloned());
                if h.kernel.exists(ai.as_str()) {
                    return done(json!({"id": ai, "root": ai, "saved": saved, "name": named, "shared": true})).await;
                }
                let mut spec: Map<String, Value> = d.get("record").and_then(Value::as_object).cloned().unwrap_or_default();
                let mut handler = spec.get("handler_module").cloned();
                // the provider made with the AI stays when the AI goes (#632): the AI is re-made on it
                let kept = provider_id(&ai);
                if handler.as_ref().and_then(Value::as_str) != Some(CHAT_HANDLER) && h.kernel.exists(&kept) {
                    spec = chat_part(&spec);
                    spec.insert(PROVIDER_FIELD.into(), json!(kept));
                    handler = Some(json!(CHAT_HANDLER));
                }
                spec.extend(stamp);
                // what the dialog's AI is stays what it is, whatever the stamp says
                if let Some(hm) = handler {
                    spec.insert("handler_module".into(), hm);
                }
                let ai2 = ai.clone();
                Self::make_ai(
                    &h,
                    &by2,
                    &me2,
                    &ai,
                    spec,
                    peer,
                    cps::done(move |r: Result<(), String>| async move {
                        done(match r {
                            Ok(()) => json!({"id": ai2, "root": ai2, "saved": saved, "name": named, "shared": false}),
                            Err(e) => fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, format!("reopen: re-making the AI: {e}")),
                        })
                        .await;
                    }),
                )
                .await;
            }),
        )
        .await;
    }
}

#[async_trait]
impl Handler for BackendBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(&self, agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        if let verbs::Call::Reflect(_) = call {
            return Ok(Some(json!({
                "handler_module": HANDLER_MODULE,
                "sentence": MAN,
            })));
        }
        let Some(host) = Self::host(kernel) else {
            return Ok(Some(fantastic_kernel::refusal(fantastic_kernel::refusal::NOT_FOUND, "canvas: the kernel has no root")));
        };
        // EVERY OTHER VERB answers from a chain of calls (#795): the settle is deferred,
        // and the chain's last step settles it. The identity of this dispatch is captured
        // once ([`By::here`]); each call of the chain is made under it.
        let by = By::here();
        let me = agent_id.clone();
        let reply = crate::kern::cps::reply_later(kernel, |done| async move {
            match call {
                verbs::Call::Reflect(_) => unreachable!("answered above"),
                verbs::Call::Create(a) => Self::create(&host, &by, &me, a.record, done).await,
                verbs::Call::Delete(a) => Self::delete(&host, &by, &me, &a.id, done).await,
                verbs::Call::Presets(_) => Self::presets(&host, done).await,
                verbs::Call::Spawn(a) => Self::spawn(&host, &by, &me, a, done).await,
                verbs::Call::Keep(a) => Self::keep(&host, &by, a, done).await,
                verbs::Call::Rename(a) => Self::rename(&host, &by, a, done).await,
                verbs::Call::Dialogs(_) => Self::dialogs(&host, &by, done).await,
                verbs::Call::Reopen(a) => Self::reopen(&host, &by, &me, a, done).await,
                verbs::Call::NewCanvas(a) => {
                    crate::kern::web::new_canvas(
                        &host,
                        &me.0,
                        &a.name,
                        cps::done(move |r: Result<String, String>| async move {
                            done(match r {
                                Ok(id) => json!({"id": id}),
                                Err(e) => fantastic_kernel::refusal(fantastic_kernel::refusal::BAD_ARG, e),
                            })
                            .await;
                        }),
                    )
                    .await;
                }
                verbs::Call::Canvases(_) => done(Self::canvases(&host)).await,
                verbs::Call::AddMcp(a) => Self::add_mcp(&host, &by, &me, a, done).await,
                verbs::Call::ConfigureMcp(a) => Self::configure_mcp(&host, &by, a, done).await,
            }
        })
        .await;
        Ok(Some(reply))
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

    fantastic_kernel::stand_in_verbs!(fake_ai_verbs { Ping "ping" {}, Complete "complete" {} });

    #[async_trait]
    impl Handler for FakeAi {
        type Call = fake_ai_verbs::Call;

        fn name(&self) -> &str {
            self.0
        }

        fn man(&self) -> &'static str {
            "A test stand-in AI handler."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            fake_ai_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            fake_ai_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(&self, _id: &AgentId, call: Self::Call, _k: &Arc<Kernel>) -> Result<Option<Value>, BundleError> {
            Ok(match call {
                fake_ai_verbs::Call::Ping(_) => Some(json!({"pong": true})),
                fake_ai_verbs::Call::Complete(_) => Some(json!({"done": true})),
                fake_ai_verbs::Call::Reflect(_) => None,
            })
        }
    }

    /// The record of a backend that needs a field no saved spec here carries.
    mod needs_field {
        /// The one field its record requires.
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
        #[allow(dead_code)]
        pub struct NeedsField {
            /// A field the specs of these tests do not carry.
            pub needed: String,
        }

        fantastic_kernel::record!(NeedsField);
    }

    /// A backend whose record is refused at the write (a required field is missing): the make of a pair on it fails.
    struct RefusingAi;

    fantastic_kernel::stand_in_verbs!(refusing_ai_verbs { Ping "ping" {} });

    #[async_trait]
    impl Handler for RefusingAi {
        type Call = refusing_ai_verbs::Call;

        fn name(&self) -> &str {
            "ollama_backend.tools"
        }

        fn man(&self) -> &'static str {
            "A test stand-in backend whose record needs a field."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            refusing_ai_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            refusing_ai_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            &needs_field::RECORD
        }

        async fn handle(&self, _id: &AgentId, call: Self::Call, _k: &Arc<Kernel>) -> Result<Option<Value>, BundleError> {
            Ok(match call {
                refusing_ai_verbs::Call::Ping(_) => Some(json!({"pong": true})),
                refusing_ai_verbs::Call::Reflect(_) => None,
            })
        }
    }

    fantastic_kernel::stand_in_verbs!(store_verbs {
        Append "append" { rows: Vec<serde_json::Value> },
        Read "read" { kinds: Option<Vec<String>> },
    });

    /// A memory store that keeps the rows appended to it and answers a read with all of them.
    #[derive(Default)]
    struct FakeStore(std::sync::Mutex<Vec<Value>>);

    #[async_trait]
    impl Handler for FakeStore {
        type Call = store_verbs::Call;

        fn name(&self) -> &str {
            "fake_store.tools"
        }

        fn man(&self) -> &'static str {
            "A test stand-in memory store."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            store_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            store_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(&self, id: &AgentId, call: Self::Call, _k: &Arc<Kernel>) -> Result<Option<Value>, BundleError> {
            let mut rows = self.0.lock().unwrap();
            Ok(Some(match call {
                store_verbs::Call::Append(a) => {
                    rows.extend(a.rows);
                    json!({"ok": true})
                }
                store_verbs::Call::Read(_) => json!({"rows": *rows}),
                store_verbs::Call::Reflect(_) => json!({"id": id.as_str(), "sentence": "a store"}),
            }))
        }
    }

    /// `create` of a `{record}` payload.
    async fn create_rec(h: &Host, me: &AgentId, p: &Value) -> Value {
        cps::run(|d| BackendBundle::create(h, &By::Operator, me, serde_json::from_value(p["record"].clone()).expect("a child record"), d)).await
    }

    /// A host on a kernel whose root is `kernel`, the human `user`, a canvas `canvas_root`
    /// and the memory store `memory`. The test drives it as the operator.
    pub(crate) async fn host(name: &str) -> (Host, PathBuf) {
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
        kernel.bundles.register("file_bridge.tools", fantastic_file::FileBundle::default());
        kernel.bundles.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());
        let kernel = Arc::new(kernel);
        let root = Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            Map::new(),
            dir.join(".fantastic"),
            false,
        );
        fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
        kernel.set_root(root);
        let h = Host { kernel, cfg: ClientCfg::here(), root: "kernel".to_string() };
        for spec in [
            json!({"parent_id": "kernel", "id": "memory", "handler_module": fantastic_core::HANDLER_MODULE}),
            json!({"parent_id": "kernel", "id": crate::env::HUMAN, "handler_module": "user.tools"}),
            json!({"parent_id": "kernel", "id": "canvas_root", "handler_module": HANDLER_MODULE}),
        ] {
            let mut payload = spec.as_object().unwrap().clone();
            payload.insert("type".into(), json!("create_agent"));
            let out = cps::op(&h, "kernel", Value::Object(payload)).await;
            assert!(out.result.get("error").is_none(), "{:?}", out.result);
        }
        (h, dir)
    }

    pub(crate) fn record_of(h: &Host, id: &str) -> Map<String, Value> {
        h.kernel.resolve_target(id).map(|a| a.record().meta.clone()).unwrap_or_default()
    }

    pub(crate) const PV: [&str; 3] = ["complete", "watch", "capabilities"];

    pub(crate) fn rec_ai(extra: Value) -> Value {
        let mut r = json!({"handler_module": "vllm_backend.tools", "id": "helper", "memory_store_id": "memory",
            "endpoint": "http://box.example:8000/v1", "model": "m1", "kv_overflow": "evict", "context_strategy": "none"});
        r.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        r
    }

    #[tokio::test]
    async fn an_ai_made_on_a_canvas_is_a_provider_and_a_chat_naming_it() {
        let (h, dir) = host("ai").await;
        let me = AgentId::from("canvas_root");
        let out = create_rec(&h, &me, &json!({"record": rec_ai(json!({}))})).await;
        assert_eq!(out, json!({"id": "helper"}), "{out}");
        for id in ["helper", "helper_provider"] {
            let rec = h.kernel.resolve_target(id).unwrap().record();
            assert_eq!(rec.parent_id.as_deref(), Some("canvas_root"), "{id} is a child of the canvas");
        }
        let handler = |id: &str| h.kernel.resolve_target(id).unwrap().record().handler_module;
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
        // the lines, exactly, each in its holder's own record: the chat calls its store and
        // the provider's complete/watch, the provider its store; the store keeps no list
        assert_eq!(chat["can"], json!({"$parent": {"call": ["send"]}, "memory": {"call": STORE_VERBS}, "helper_provider": {"call": PV}}));
        assert_eq!(prov["can"], json!({"memory": {"call": STORE_VERBS}}));
        assert!(record_of(&h, "memory").get("can").is_none(), "the store holds no line");
        // asked again as either handler, it is the same child
        let again = create_rec(&h, &me, &json!({"record": rec_ai(json!({}))})).await;
        assert_eq!(again, json!({"id": "helper", "existed": true}), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gm rule 7, Q39: the canvas `create` and `delete` act for the asker, so the kernel checks
    /// them as the asker. A call line on the canvas node gives no create; the asker's own create
    /// line is what lets it delete an agent it selects; rule 5 holds through the verb.
    #[tokio::test]
    async fn a_canvas_create_or_delete_is_checked_as_the_asker() {
        let (h, dir) = host("asker").await;
        for (id, parent) in [("asker", "kernel"), ("self_asker", "canvas_root"), ("seeded", "canvas_root")] {
            let r = cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": "fake_store.tools"}))
                .await;
            assert!(r.result.get("error").is_none(), "{:?}", r.result);
        }
        let ask = |who: &'static str, p: Value| {
            let h = &h;
            async move {
                let item = fantastic_kernel::Item::new(AgentId::from("canvas_root"), p);
                fantastic_kernel::test_support::as_agent(who, fantastic_kernel::test_support::settled(&h.kernel, vec![item])).await.remove(0).result
            }
        };
        // a call line on the canvas node, no create line: the create is refused, nothing is made
        fantastic_kernel::test_support::grant(&h.kernel, "asker", "canvas_root", json!({"call": ["create", "delete"]}));
        let r = ask("asker", json!({"type": "create", "record": {"handler_module": "fake_store.tools", "id": "made"}})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert!(!h.kernel.exists("made"), "the asker holds no create line");
        let r = ask("asker", json!({"type": "delete", "id": "seeded"})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert!(h.kernel.exists("seeded"), "the asker holds no delete on `seeded`");
        // a create line on the canvas that selects `seeded` and the asker itself
        fantastic_kernel::test_support::grant(&h.kernel, "self_asker", "$parent", json!({"call": ["delete"]}));
        fantastic_kernel::test_support::grant(&h.kernel, "self_asker", "canvas_root", json!({"create": {"who": ["*"], "what": "*"}}));
        // rule 5: never its own record, even through the canvas verb
        let r = ask("self_asker", json!({"type": "delete", "id": "self_asker"})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert!(h.kernel.exists("self_asker"), "nothing deletes its own record");
        // its own line covers an agent it selects
        let r = ask("self_asker", json!({"type": "delete", "id": "seeded"})).await;
        assert!(r.get("error").is_none(), "{r}");
        assert!(!h.kernel.exists("seeded"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gm rule 7 (F1): a canvas `create` asked for by an agent is what BOTH allow. The asker's
    /// lines must allow the child as asked (its bundle, id, fields and the lines it supplies);
    /// the canvas node then creates it as itself, so its own create line selects the bundle,
    /// forces its values (the garage's forced `can`) and must hold every line handed out.
    #[tokio::test]
    async fn a_canvas_create_is_what_the_canvas_and_the_asker_both_allow() {
        let (h, dir) = host("both").await;
        for (id, parent, hm) in [
            ("garage", "canvas_root", HANDLER_MODULE),
            ("sub", "canvas_root", HANDLER_MODULE),
            ("leg", "kernel", "fake_store.tools"),
            ("narrow", "kernel", "fake_store.tools"),
            ("keyed", "kernel", "fake_store.tools"),
            ("keys", "kernel", "fake_store.tools"),
        ] {
            let r = cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm})).await;
            assert!(r.result.get("error").is_none(), "{:?}", r.result);
        }
        let g = |holder: &str, target: &str, level: Value| fantastic_kernel::test_support::grant(&h.kernel, holder, target, level);
        let star = json!({"who": ["*"], "what": "*"});
        // the legs: a create line on the user space (it calls the canvases below it)
        g("leg", "canvas_root", json!({"create": star}));
        g("keyed", "canvas_root", json!({"create": star}));
        g("keyed", "keys", json!({"call": ["ping"]}));
        g("narrow", "canvas_root", json!({"create": {"who": ["n_*"], "what": {"fake_store.tools": {}}}}));
        // its create line selects no canvas: calling the garage is its own line
        g("narrow", "garage", json!({"call": ["create"]}));
        // the canvases: the garage forces a `can`, `sub` frees `can`; canvas_root caps their key line
        g("garage", "garage", json!({"create": {"who": ["*"], "what": {"fake_store.tools": {"note": "*", "can": {"keys": {"call": ["ping"]}}}}}}));
        g("garage", "keys", json!({"call": ["ping"]}));
        g("sub", "sub", json!({"create": {"who": ["*"], "what": {"fake_store.tools": {"can": "*"}}}}));
        g("sub", "keys", json!({"call": ["ping"]}));
        g("canvas_root", "keys", json!({"call": ["ping"]}));
        let ask = |who: &'static str, node: &'static str, record: Value| {
            let h = &h;
            async move {
                let item = fantastic_kernel::Item::new(AgentId::from(node), json!({"type": "create", "record": record}));
                fantastic_kernel::test_support::as_agent(who, fantastic_kernel::test_support::settled(&h.kernel, vec![item])).await.remove(0).result
            }
        };
        // both allow it: made by the garage, its forced `can` over the child, the asked field kept
        let r = ask("leg", "garage", json!({"handler_module": "fake_store.tools", "id": "s1", "note": "n"})).await;
        assert!(r.get("error").is_none(), "{r}");
        let rec = h.kernel.resolve_target("s1").expect("made").record();
        assert_eq!(rec.parent_id.as_deref(), Some("garage"));
        assert_eq!(rec.meta["can"], json!({"keys": {"call": ["ping"]}}), "the garage line's forced can");
        assert_eq!(rec.meta["note"], json!("n"));
        // the asker allows it, the garage's line does not select the bundle: refused
        let r = ask("leg", "garage", json!({"handler_module": "user.tools", "id": "u1"})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert!(!h.kernel.exists("u1"));
        // the garage allows it, the asker's `who` does not: refused; inside its `who`, made
        let r = ask("narrow", "garage", json!({"handler_module": "fake_store.tools", "id": "s2"})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert!(!h.kernel.exists("s2"));
        let r = ask("narrow", "garage", json!({"handler_module": "fake_store.tools", "id": "n_1"})).await;
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(h.kernel.resolve_target("n_1").expect("made").record().meta["can"], json!({"keys": {"call": ["ping"]}}));
        // a field the garage frees and the asker's line does not name: refused
        let r = ask("narrow", "garage", json!({"handler_module": "fake_store.tools", "id": "n_2", "note": "x"})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert!(!h.kernel.exists("n_2"));
        // a line the asker supplies: the canvas holds it, the asker does not — refused
        let r = ask("leg", "sub", json!({"handler_module": "fake_store.tools", "id": "c1", "can": {"keys": {"call": ["ping"]}}})).await;
        assert!(r.get("error").is_some(), "{r}");
        assert!(!h.kernel.exists("c1"));
        // an asker that holds it too: made, by the canvas node, with the line
        let r = ask("keyed", "sub", json!({"handler_module": "fake_store.tools", "id": "c2", "can": {"keys": {"call": ["ping"]}}})).await;
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(h.kernel.resolve_target("c2").expect("made").record().meta["can"], json!({"keys": {"call": ["ping"]}}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F5: code inside a handler says whose send it makes. `Host::agent_then` is the running
    /// agent's own, checked against its lines (never the operator's); in the shell's operator
    /// scope it is the operator's; with no scope at all it is refused (lost identity is never
    /// the operator). `Host::asker_then` is the asker's: the operator when the operator asked.
    #[tokio::test]
    async fn an_agent_send_inside_a_dispatch_is_that_agents_and_the_shells_is_the_operators() {
        let (h, dir) = host("whose").await;
        for id in ["inside", "box", "box2"] {
            let r = cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": id, "handler_module": "fake_store.tools"})).await;
            assert!(r.result.get("error").is_none(), "{:?}", r.result);
        }
        let read = json!({"type": "read"});
        // the shell's operator scope: the operator's
        assert!(fantastic_kernel::send::as_operator(cps::agent_op(&h, "box", read.clone())).await.result.get("error").is_none());
        // no scope at all (a bare spawn): nobody's, refused
        assert!(cps::agent_op(&h, "box", read.clone()).await.result.get("error").is_some());
        // inside `inside`'s dispatch: its own send, and it holds no line on `box`
        let r = fantastic_kernel::test_support::as_agent("inside", cps::agent_op(&h, "box", read.clone())).await.result;
        assert_eq!(r, fantastic_kernel::unknown_agent("box"), "{r}");
        fantastic_kernel::test_support::grant(&h.kernel, "inside", "box", json!({"call": ["read"]}));
        let r = fantastic_kernel::test_support::as_agent("inside", cps::agent_op(&h, "box", read.clone())).await.result;
        assert!(r.get("error").is_none(), "{r}");
        // the asker's send, asked by the operator: the operator's, where the agent holds nothing
        let r = fantastic_kernel::test_support::as_agent("inside", cps::agent_op(&h, "box2", read.clone())).await.result;
        assert_eq!(r, fantastic_kernel::unknown_agent("box2"), "{r}");
        let r = fantastic_kernel::test_support::as_agent("inside", async { cps::asker_op(&h, &fantastic_kernel::Asker::current(), "box2", read.clone()).await }).await.result;
        assert!(r.get("error").is_none(), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chat_refused_deletes_the_provider_just_made() {
        let (h, dir) = host("refused").await;
        let me = AgentId::from("canvas_root");
        let first = cps::run(|d| BackendBundle::make_ai(&h, &By::Operator, &me, "taken", rec_ai(json!({})).as_object().unwrap().clone(), Map::new(), d)).await;
        assert!(first.is_ok(), "{first:?}");
        // the chat id is taken now: the second chat is refused, the provider it made first goes
        cps::op(&h, "kernel", json!({"type": "delete_agent", "id": "taken_provider"})).await;
        let second = cps::run(|d| BackendBundle::make_ai(&h, &By::Operator, &me, "taken", rec_ai(json!({})).as_object().unwrap().clone(), Map::new(), d)).await;
        assert!(second.is_err(), "the chat `taken` exists");
        assert!(!h.kernel.exists("taken_provider"), "the provider of the refused chat is gone");
        assert!(h.kernel.exists("taken"), "the first chat is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_takes_the_chat_and_its_provider_stays() {
        let (h, dir) = host("delete").await;
        let me = AgentId::from("canvas_root");
        let made = create_rec(&h, &me, &json!({"record": rec_ai(json!({}))})).await;
        assert_eq!(made, json!({"id": "helper"}), "{made}");
        let out = cps::run(|d| BackendBundle::delete(&h, &By::Operator, &me, "helper", d)).await;
        assert!(out.get("error").is_none(), "{out}");
        assert!(!h.kernel.exists("helper"), "the chat is gone");
        assert!(h.kernel.exists("helper_provider"), "its provider stays until it is deleted itself");
        // a new pair is made the same way
        let again = cps::run(|d| BackendBundle::make_ai(&h, &By::Operator, &me, "again", rec_ai(json!({})).as_object().unwrap().clone(), Map::new(), d)).await;
        assert!(again.is_ok(), "{again:?}");
        assert!(h.kernel.exists("again_provider"));
        // the provider itself is deleted like any agent
        let out = cps::run(|d| BackendBundle::delete(&h, &By::Operator, &me, "helper_provider", d)).await;
        assert!(out.get("error").is_none(), "{out}");
        assert!(!h.kernel.exists("helper_provider"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chat_asked_for_names_its_provider_and_holds_its_line_on_it() {
        let (h, dir) = host("chat-only").await;
        let me = AgentId::from("canvas_root");
        let prov = cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "prov", "handler_module": "vllm_backend.tools"}))
            .await;
        assert!(prov.result.get("error").is_none(), "{:?}", prov.result);
        let out = create_rec(
            &h,
            &me,
            &json!({"record": {"handler_module": "ai_chat.tools", "id": "mine", "provider_id": "prov", "memory_store_id": "memory"}}),
        )
        .await;
        assert_eq!(out, json!({"id": "mine"}), "{out}");
        assert!(!h.kernel.exists("mine_provider"), "no second provider");
        assert_eq!(record_of(&h, "mine")["can"]["prov"], json!({"call": PV}));
        assert!(record_of(&h, "prov").get("can").is_none(), "the provider keeps no list of callers");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chat_naming_no_provider_is_refused_and_a_taken_id_is_refused() {
        let (h, dir) = host("admit").await;
        let me = AgentId::from("canvas_root");
        // a chat naming an agent that is no provider is refused by name, and that agent is untouched
        let r = create_rec(&h, &me, &json!({"record": {"handler_module": "ai_chat.tools", "id": "mine", "provider_id": "memory", "memory_store_id": "memory"}})).await;
        assert!(r["error"].as_str().is_some_and(|e| e.contains("`memory` is not a provider")), "{r}");
        assert!(!h.kernel.exists("mine"));
        // a chat whose id is taken is refused
        let prov = cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "prov", "handler_module": "vllm_backend.tools"}))
            .await;
        assert!(prov.result.get("error").is_none(), "{:?}", prov.result);
        let ok = create_rec(&h, &me, &json!({"record": {"handler_module": "ai_chat.tools", "id": "other", "provider_id": "prov", "memory_store_id": "memory"}})).await;
        assert_eq!(ok, json!({"id": "other"}), "{ok}");
        let again = cps::run(|d| BackendBundle::make_ai(&h, &By::Operator, &me, "other", json!({"handler_module": "ai_chat.tools", "provider_id": "prov", "memory_store_id": "memory"}).as_object().unwrap().clone(), Map::new(), d)).await;
        assert!(again.is_err(), "the chat `other` exists");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keep_records_the_pair_as_one_provider_shaped_record() {
        let (h, dir) = host("keep").await;
        let me = AgentId::from("canvas_root");
        let made = cps::run(|d| BackendBundle::make_ai(&h, &By::Operator, &me, "helper", rec_ai(json!({})).as_object().unwrap().clone(), Map::new(), d)).await;
        assert!(made.is_ok(), "{made:?}");
        // the kept dialog is a row on the project's store: that store keeps rows
        cps::op(&h, "kernel", json!({"type": "delete_agent", "id": "memory"})).await;
        let store = cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "memory", "handler_module": "fake_store.tools"}))
            .await;
        assert!(store.result.get("error").is_none(), "{:?}", store.result);
        fantastic_kernel::test_support::grant(&h.kernel, crate::env::HUMAN, "memory", json!({"call": ["*"]}));
        // the shell's own calls: the operator's
        let out = fantastic_kernel::send::as_operator(cps::run(|d| BackendBundle::keep(&h, &By::Operator, verbs::Keep { id: "helper".into(), name: Some("kept".into()) }, d))).await;
        assert_eq!(out["kept"], json!("helper"), "{out}");
        let rows = fantastic_kernel::send::as_operator(cps::run(|d| BackendBundle::dialogs_map(&h, &By::Operator, d))).await.unwrap();
        let rec = &rows["helper"]["record"];
        assert_eq!(rec["handler_module"], json!("vllm_backend.tools"), "the provider's handler, not the chat's");
        assert_eq!(rec["endpoint"], json!("http://box.example:8000/v1"), "the provider's fields");
        assert_eq!(rec["context_strategy"], json!("none"), "under the chat's");
        assert!(rec.get("provider_id").is_none(), "the saved set-up names no provider: it is made again at reopen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keep_writes_the_saved_set_up_on_the_dialog_row_and_not_on_the_chat_record() {
        let (h, dir) = host("keep-saved").await;
        let me = AgentId::from("canvas_root");
        let made = cps::run(|d| BackendBundle::make_ai(&h, &By::Operator, &me, "helper", rec_ai(json!({})).as_object().unwrap().clone(), Map::new(), d)).await;
        assert!(made.is_ok(), "{made:?}");
        assert!(meta_has_no_saved(&h, "helper"), "a made chat carries no `saved` field");
        cps::op(&h, "kernel", json!({"type": "delete_agent", "id": "memory"})).await;
        let store = cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "memory", "handler_module": "fake_store.tools"}))
            .await;
        assert!(store.result.get("error").is_none(), "{:?}", store.result);
        fantastic_kernel::test_support::grant(&h.kernel, crate::env::HUMAN, "memory", json!({"call": ["*"]}));
        // what `spawn` hands over: the set-up's name, kept on the row (ai_settings reads it from here)
        // the shell's own calls: the operator's
        let out = fantastic_kernel::send::as_operator(cps::run(|d| BackendBundle::keep_with(&h, &By::Operator, "helper".into(), None, Some("mine".into()), d))).await;
        assert_eq!(out["kept"], json!("helper"), "{out}");
        let rows = fantastic_kernel::send::as_operator(cps::run(|d| BackendBundle::dialogs_map(&h, &By::Operator, d))).await.unwrap();
        assert_eq!(rows["helper"]["saved"], json!("mine"), "{rows:?}");
        assert!(rows["helper"]["record"].get("saved").is_none(), "{rows:?}");
        assert!(meta_has_no_saved(&h, "helper"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn meta_has_no_saved(h: &Host, id: &str) -> bool {
        BackendBundle::meta_of(h, id).is_some_and(|m| !m.contains_key("saved"))
    }

    #[tokio::test]
    async fn spawn_instance_refuses_a_bad_tui_with_reason_bad_arg() {
        let (h, dir) = host("bad-tui").await;
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
        let out = cps::run(|d| BackendBundle::spawn_instance_in(&h, &By::Operator, &me, "fakeb", Map::new(), &state, d)).await;
        assert_eq!(out["reason"], "bad_arg", "{out}");
        let e = out["error"].as_str().unwrap();
        assert!(e.contains("view.tui[0]") && e.contains("show"), "{e}");
        // a good declaration spawns, and the instance carries it
        std::fs::write(bundle.join("bundle.yaml"), doc("kv")).unwrap();
        let out = cps::run(|d| BackendBundle::spawn_instance_in(&h, &By::Operator, &me, "fakeb", Map::new(), &state, d)).await;
        assert!(out.get("error").is_none(), "{out}");
        let id = out["id"].as_str().unwrap().to_string();
        assert_eq!(record_of(&h, &id)["view"]["tui"][0]["show"], "kv");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_canvas_node_checks_every_call_against_its_declared_verbs() {
        let (h, dir) = host("declared").await;
        let ask = |p: Value| {
            let h = &h;
            async move { cps::op(h, "canvas_root", p).await.result }
        };
        let why = |r: &Value| (r["reason"].as_str().map(str::to_string), r["arg"].as_str().map(str::to_string));
        let r = ask(json!({"type": "zzz_nope"})).await;
        assert_eq!(r["reason"], "unhandled", "{r}");
        for v in verbs::VERBS.iter().map(|v| v.name).chain(["reflect"]) {
            assert!(r["error"].as_str().unwrap().contains(v), "{v} in {r}");
        }
        for v in verbs::VERBS {
            let r = ask(json!({"type": v.name, "zzz_unknown": 1})).await;
            assert_eq!(why(&r), (Some("bad_arg".into()), Some("zzz_unknown".into())), "{}: {r}", v.name);
            let required: Vec<String> = fantastic_kernel::verbs::args(v.schema()).into_iter().filter(|a| a.required).map(|a| a.name).collect();
            if !required.is_empty() {
                let r = ask(json!({"type": v.name})).await;
                assert_eq!(r["reason"], "missing_arg", "{}: {r}", v.name);
                assert!(required.iter().any(|n| r["arg"] == json!(n)), "{}: {r}", v.name);
            }
        }
        // wrongly typed, never coerced
        for (p, arg) in [
            (json!({"type": "create", "record": "x"}), "record"),
            (json!({"type": "delete", "id": 3}), "id"),
            (json!({"type": "spawn", "saved": ""}), "saved"),
            (json!({"type": "spawn", "stamp": []}), "stamp"),
            (json!({"type": "spawn", "peer_grants": "all"}), "peer_grants"),
            (json!({"type": "keep", "id": "a", "name": 3}), "name"),
            (json!({"type": "rename", "id": "a", "name": 3}), "name"),
            (json!({"type": "reopen", "id": "a", "stamp": 1}), "stamp"),
            (json!({"type": "add_mcp", "url": "  "}), "url"),
            (json!({"type": "configure_mcp", "id": "a", "key": 3}), "key"),
            (json!({"type": "configure_mcp", "id": "a", "url": ""}), "url"),
            (json!({"type": "configure_mcp", "id": "a", "url": "  "}), "url"),
        ] {
            let r = ask(p.clone()).await;
            assert_eq!(why(&r), (Some("bad_arg".into()), Some(arg.into())), "{p}: {r}");
        }
        // an empty required text is a wrong value
        let r = ask(json!({"type": "new_canvas", "name": ""})).await;
        assert_eq!(why(&r), (Some("bad_arg".into()), Some("name".into())), "{r}");
        let r = ask(json!({"type": "create", "record": {}})).await;
        assert_eq!(why(&r), (Some("missing_arg".into()), Some("record.handler_module".into())), "{r}");
        // a structural field makes a different agent: refused by the declared schema
        for f in ["type", "parent_id", "guid", "agents"] {
            let r = ask(json!({"type": "create", "record": {"handler_module": "file_bridge.tools", f: "x"}})).await;
            assert_eq!(why(&r), (Some("bad_arg".into()), Some(format!("record.{f}"))), "{f}: {r}");
        }
        for f in ["type", "id", "parent_id", "guid", "handler_module", "agents"] {
            for (verb, extra) in [("spawn", json!({})), ("reopen", json!({"id": "a"}))] {
                let mut call = extra;
                call["type"] = json!(verb);
                call["stamp"] = json!({f: "x"});
                let r = ask(call).await;
                assert_eq!(why(&r), (Some("bad_arg".into()), Some(format!("stamp.{f}"))), "{verb} {f}: {r}");
            }
        }
        // the record declares `main` and no other field
        let bundle = &BackendBundle as &dyn fantastic_kernel::Bundle;
        let fields = |v: Value| v.as_object().unwrap().clone();
        assert!(bundle.check_record("update_agent", &fields(json!({"main": true}))).is_ok());
        assert!(bundle.check_record("update_agent", &fields(json!({"main": "yes"}))).is_err());
        assert!(bundle.check_record("update_agent", &fields(json!({"stray": 1}))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_ai_naming_a_store_of_its_own_holds_its_line_on_that_store() {
        let (h, dir) = host("own").await;
        cps::op(&h, "kernel", json!({"type": "create_agent", "parent_id": "kernel", "id": "own_store", "handler_module": fantastic_core::HANDLER_MODULE}))
            .await;
        let out = create_rec(
            &h,
            &AgentId::from("canvas_root"),
            &json!({"record": {"handler_module": "vllm_backend.tools", "id": "helper2", "memory_store_id": "own_store"}}),
        )
        .await;
        assert_eq!(out, json!({"id": "helper2"}), "{out}");
        assert_eq!(record_of(&h, "helper2")["memory_store_id"], json!("own_store"));
        assert_eq!(record_of(&h, "helper2")["can"]["own_store"], json!({"call": STORE_VERBS}));
        assert!(record_of(&h, "helper2")["can"].get("memory").is_none(), "no line on the project's store");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_plain_agent_made_on_a_canvas_names_no_store() {
        let (h, dir) = host("plain").await;
        let out = create_rec(
            &h,
            &AgentId::from("canvas_root"),
            &json!({"record": {"handler_module": "file_bridge.tools", "id": "plain_exec"}}),
        )
        .await;
        assert_eq!(out.get("id"), Some(&json!("plain_exec")), "{out}");
        let meta = record_of(&h, "plain_exec");
        assert!(meta.get("memory_store_id").is_none());
        assert!(meta.get("can").and_then(|c| c.get("memory")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pair_specs_split_the_fields_and_merge_the_callers_lines_over_the_defaults() {
        let me = AgentId::from("canvas_root");
        let spec = json!({
            "handler_module": "vllm_backend.tools",
            "endpoint": "http://h/v1",
            "credentials": {"store": "credentials", "ref": "vllm/h"},
            "model": "m1",
            "caching": "auto",
            "temperature": 0.5,
            "max_tokens": 4096,
            "can": {"tool_a": {"call": ["run"]}, "memory": {"call": ["read"]}},
        });
        let (prov, chat, asked) = BackendBundle::pair_specs(
            &me,
            "helper",
            "memory",
            Some("prompt_file"),
            spec.as_object().unwrap().clone(),
            Map::from_iter([("peer_b".to_string(), json!({"call": ["send"]}))]),
        );
        let prov = prov.expect("a provider");
        assert_eq!((prov["type"].clone(), prov["parent_id"].clone(), prov["id"].clone()), (json!("create_agent"), json!("canvas_root"), json!("helper_provider")));
        assert_eq!(prov["handler_module"], json!("vllm_backend.tools"));
        assert_eq!(prov["endpoint"], json!("http://h/v1"));
        assert_eq!(prov["model"], json!("m1"));
        assert!(prov.get("caching").is_none() && prov.get("provider_id").is_none());
        // the request knobs are the chat's (#632)
        assert!(prov.get("temperature").is_none() && prov.get("max_tokens").is_none(), "{prov:?}");
        assert_eq!((chat["temperature"].clone(), chat["max_tokens"].clone()), (json!(0.5), json!(4096)));
        // the key's `get` is the provider's line; the caller's tool lines are the chat's
        assert_eq!(prov["can"], json!({"memory": {"call": STORE_VERBS}, "credentials": {"call": ["get"]}}));
        assert_eq!(chat["handler_module"], json!("ai_chat.tools"));
        assert_eq!(chat["id"], json!("helper"));
        assert_eq!(chat["provider_id"], json!("helper_provider"));
        assert_eq!(chat["memory_store_id"], json!("memory"));
        assert_eq!(chat["caching"], json!("auto"));
        assert!(chat.get("endpoint").is_none() && chat.get("credentials").is_none());
        // the caller's own `memory` line wins a clash; the store is named either way
        assert_eq!(chat["can"]["memory"], json!({"call": ["read"]}));
        assert_eq!(chat["can"]["tool_a"], json!({"call": ["run"]}));
        assert_eq!(chat["can"]["peer_b"], json!({"call": ["send"]}));
        assert_eq!(chat["can"]["helper_provider"], json!({"call": ["complete", "watch", "capabilities"]}));
        assert!(chat["can"].get("credentials").is_none());
        assert!(chat.get("callable_by").is_none(), "no target keeps a list of callers");
        // the lines a new chat cell gets: its canvas's `send`, the system block's `read` and `watch`
        assert_eq!(chat["can"]["$parent"], json!({"call": ["send"]}));
        assert_eq!(chat["can"]["prompt_file"], json!({"call": ["read", "watch"]}));
        // the lines the ASKER supplied, apart: the caller's (winning a clash) and the peer's
        assert_eq!(
            Value::Object(asked),
            json!({"peer_b": {"call": ["send"]}, "tool_a": {"call": ["run"]}, "memory": {"call": ["read"]}})
        );
        // no system block view here: no line on it; the store line carries `copy` for a fork
        let (_, chat, _) = BackendBundle::pair_specs(&me, "h2", "memory", None, json!({"handler_module": "vllm_backend.tools"}).as_object().unwrap().clone(), Map::new());
        assert!(chat["can"].get("prompt_file").is_none());
        assert_eq!(chat["can"]["memory"], json!({"call": ["append", "read", "copy"]}));
    }

    /// Every request knob (#632) a spec carries lands on the chat and never on its provider.
    #[test]
    fn pair_specs_routes_every_knob_to_the_chat() {
        let me = AgentId::from("canvas_root");
        let mut spec = Map::new();
        spec.insert("handler_module".into(), json!("vllm_backend.tools"));
        for k in fantastic_ai_core::record::KNOBS {
            spec.insert(k.to_string(), json!(format!("v_{k}")));
        }
        let (prov, chat, _) = BackendBundle::pair_specs(&me, "helper", "memory", None, spec, Map::new());
        let prov = prov.expect("a provider");
        for k in fantastic_ai_core::record::KNOBS {
            assert_eq!(chat.get(k), Some(&json!(format!("v_{k}"))), "{k} on the chat");
            assert!(prov.get(k).is_none(), "{k} leaked to the provider: {prov:?}");
        }
    }
}
