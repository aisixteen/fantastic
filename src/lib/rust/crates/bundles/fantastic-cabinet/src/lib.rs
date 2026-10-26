//! Cabinets — a master AI opens cabinets: canvases, each with one worker AI.
//!
//! Port of python `cabinet/tools.py` (the reference runtime): verb for verb, with
//! the same refusals, the same grants and the same wiring. See `readme.md`.
//!
//! THE SHAPE. One MANAGER (a `cabinet.tools` agent the operator makes once) owns the
//! CABINETS as its children, and names the CANVAS that spawns them (`canvas_id`). A
//! cabinet is a CANVAS of its own: `open` asks that canvas (`new_canvas`) to make it,
//! so the cabinet's canvas is a CHILD of the spawning canvas, nested in the hierarchy.
//! The cabinet's record is the live status cell (`view.html`) that the main canvas
//! shows and that opens the cabinet's canvas. The cabinet owns ONE WORKER AI as its
//! child. The worker's EQUIPMENT is placed on the cabinet's canvas with `create` and
//! `place`, and reached through the canvas's grants and nothing else.
//!
//! NOTHING IS MADE OR CHANGED BEHIND A GRANT. The manager makes and edits every record
//! by a gated send to the ROOT (`create_agent`, `update_agent`, `delete_agent`), as
//! itself. The operator grants the manager those verbs on the root. Each record's
//! grants are restated whole, because `update_agent` replaces them.
//!
//! SERIAL WRITES. `open_cabinet`, `close_cabinet` and `conclude` run ONE AT A TIME per
//! manager ([`manager_lock`]): each reads whole grant maps and writes them back.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{py_repr, py_repr_value, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::lifecycle::token_hex;
use fantastic_kernel::refusal::{
    bad, missing_hint, refusal, refusal_hint, with, BAD_ARG, MISSING_ARG, UNHANDLED, UNKNOWN_AGENT,
};
use fantastic_kernel::{current_origin, AgentId, Item, Kernel, RESERVED_RECORD_KEYS};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "cabinet.tools";

/// readme.md — the pattern, the wiring and the operator's grants.
pub const README: &str = include_str!("readme.md");

/// The cabinet's live panel, one page per cabinet (`__CABINET_ID__` is its id).
const VIEW_HTML: &str = include_str!("view.html");

const ROLE: &str = "cabinet_role";
const MANAGER: &str = "manager";
const CABINET: &str = "cabinet";

/// The worker's own report up: the only verb a worker sends its cabinet.
const STATUS_VERB: &str = "status";

/// The canvas verb that makes a child canvas: the manager asks the spawning canvas for it.
const NEW_CANVAS_VERB: &str = "new_canvas";

/// The one verb a cabinet's canvas holds on the root: it makes the equipment its worker
/// `create`s, as itself.
const CREATE_VERB: &str = "create_agent";

/// What a worker may do on its cabinet's canvas: place equipment (`create`) and place
/// views (`place`). Nothing else on the canvas.
const CANVAS_VERBS: [&str; 2] = ["create", "place"];

/// The manager's verbs that wire or unwire: each runs under the manager's lock.
const SERIAL_VERBS: [&str; 3] = ["open_cabinet", "close_cabinet", "conclude"];

/// What one worker may send another worker, or the master: messages, and nothing else.
const MESSAGE_VERBS: [&str; 2] = ["send", "reflect"];

/// Record keys a worker does NOT copy from its backend (python `_NOT_COPIED`).
const NOT_COPIED: &[&str] = &[
    "id",
    "parent_id",
    "can_call",
    "callable_by",
    "description",
    "display_name",
    "system_prompt",
    "system_prompt_file",
    "pre_system_prompt",
    "post_system_prompt",
    "view",
    "delete_lock",
    "root",
    "children_dir",
    "watch",
    "ingress_rule",
    "main",
    ROLE,
    "task",
    "worker",
    "master",
    "canvas",
    "status",
    "cabinet_peers",
    "max_cabinets",
    "canvas_id",
];

/// One lock per manager id (python `_LOCKS`, keyed by the manager): see [`manager_lock`].
/// An entry leaves the table when its last holder lets go, so a deleted manager leaves
/// nothing behind.
static MANAGER_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The keys a `setup=` may NOT carry: wiring the manager writes itself.
fn is_wiring(key: &str) -> bool {
    NOT_COPIED.contains(&key)
        && !matches!(
            key,
            "system_prompt" | "pre_system_prompt" | "system_prompt_file"
        )
}

const NEW_BACKEND_WARNING: &str = "a NEW backend: its endpoint, key or handler is not the existing one's. Creating a backend is discouraged — its credentials, cost and limits stay where their owner set them when a worker connects to an existing backend.";

/// A record as python's `kernel.get(id)` reads it: `id`, `handler_module`,
/// `parent_id`, then the meta.
type Rec = Map<String, Value>;

/// The record of `id`, or `None` when no such agent is live.
fn record(kernel: &Kernel, id: &str) -> Option<Rec> {
    let agent = kernel
        .agents
        .get(&AgentId::from(id))
        .map(|e| Arc::clone(&e))?;
    let r = agent.record();
    let mut out = Map::new();
    out.insert("id".into(), json!(r.id));
    out.insert(
        "handler_module".into(),
        r.handler_module.map_or(Value::Null, Value::String),
    );
    out.insert(
        "parent_id".into(),
        r.parent_id.map_or(Value::Null, Value::String),
    );
    out.extend(r.meta);
    Some(out)
}

fn str_of<'a>(rec: &'a Rec, key: &str) -> &'a str {
    rec.get(key).and_then(Value::as_str).unwrap_or("")
}

fn root_id(kernel: &Kernel) -> String {
    kernel.root().map(|r| r.id.0.clone()).unwrap_or_default()
}

/// Python `str.strip()[:n]` — the first `n` characters of the trimmed text.
fn head(s: &str, n: usize) -> String {
    s.trim().chars().take(n).collect()
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python `_SLUG.sub("_", name.strip().lower()).strip("_")` with `[^a-z0-9]+`.
fn slug_of(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// The lock of `manager`, held by the guard for the whole read-wire-refresh of a serial
/// verb. When the guard drops with no other task waiting (the table and the guard hold
/// the only two references), the entry is removed.
async fn manager_lock(manager: &str) -> ManagerGuard {
    let lock = {
        let mut locks = MANAGER_LOCKS
            .lock()
            .expect("cabinet manager locks poisoned");
        Arc::clone(locks.entry(manager.to_string()).or_default())
    };
    ManagerGuard {
        manager: manager.to_string(),
        _held: lock.lock_owned().await,
    }
}

/// The held lock of one manager (see [`manager_lock`]).
struct ManagerGuard {
    manager: String,
    _held: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for ManagerGuard {
    fn drop(&mut self) {
        let mut locks = MANAGER_LOCKS
            .lock()
            .expect("cabinet manager locks poisoned");
        // the table's reference and `_held`'s: nobody else waits on this lock
        if locks
            .get(&self.manager)
            .is_some_and(|l| Arc::strong_count(l) == 2)
        {
            locks.remove(&self.manager);
        }
    }
}

/// Python's `repr(list_of_str)`.
fn py_list(items: &[&str]) -> String {
    let parts: Vec<String> = items.iter().map(|s| py_repr(s)).collect();
    format!("[{}]", parts.join(", "))
}

/// The error text of a reply, when it carries a truthy one.
fn error_of(res: &Value) -> Option<String> {
    res.get("error")
        .filter(|e| py_truthy(e))
        .map(|e| e.as_str().map_or_else(|| py_repr_value(e), str::to_string))
}

/// Whether a reply says a delete went through.
fn deleted_ok(res: &Value) -> bool {
    res.get("deleted").is_some_and(py_truthy)
}

// ─── the one gated write path ────────────────────────────────────

/// One send as the calling agent (the dispatch identity). A failed or refused send
/// is an object with `error`.
async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    let ty = payload.get("type").cloned().unwrap_or(Value::Null);
    let out = kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0);
    match out.result {
        Value::Object(map) => Value::Object(map),
        _ => json!({"error": format!("{target} {}: {:?}", py_repr_value(&ty), out.status)}),
    }
}

/// A tree verb (create / update / delete), sent to the root, gated as this agent.
async fn tree(kernel: &Arc<Kernel>, payload: Value) -> Value {
    call(kernel, &root_id(kernel), payload).await
}

/// Edit `holder`'s grants, then its other fields. Each `(key, verbs)` pair sets ONE key of a
/// grant map, MERGED into what the record holds at the write (a key another writer put there
/// stays). Both maps go in one locked, once-validated step (`Kernel::edit_grants`); the other
/// fields follow in an `update_agent`. Returns the refusal's error, or `None`.
async fn wire(
    kernel: &Arc<Kernel>,
    holder: &str,
    can_call: &[(String, Value)],
    callable_by: &[(String, Value)],
    fields: &[(&str, Value)],
) -> Option<String> {
    if record(kernel, holder).is_none() {
        return Some(format!("`{holder}` does not exist"));
    }
    // BOTH grant maps merge in ONE step under the record's lock and are checked once: a
    // concurrent writer's entry survives, and a shape refused in either map writes neither.
    // The other fields go in a separate `update_agent` once the grants hold (a refusal
    // there leaves the grants written).
    if !can_call.is_empty() || !callable_by.is_empty() {
        let id = fantastic_kernel::AgentId::from(holder);
        let r = kernel.edit_grants(&id, |cc, cb| {
            (merge_grants(cc, can_call), merge_grants(cb, callable_by))
        });
        if let Err(e) = r {
            return Some(e);
        }
    }
    if fields.is_empty() {
        return None;
    }
    let mut upd = Map::new();
    upd.insert("type".into(), json!("update_agent"));
    upd.insert("id".into(), json!(holder));
    for (key, value) in fields {
        upd.insert((*key).to_string(), value.clone());
    }
    error_of(&tree(kernel, Value::Object(upd)).await)
}

/// Merge `(key, verbs)` pairs into one grant map; whether it changed.
fn merge_grants(map: &mut Map<String, Value>, pairs: &[(String, Value)]) -> bool {
    let mut changed = false;
    for (key, verbs) in pairs {
        if map.get(key) != Some(verbs) {
            map.insert(key.clone(), verbs.clone());
            changed = true;
        }
    }
    changed
}

/// `(key, verbs)` pairs from literals.
fn grants(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// Delete a half-made cabinet and its canvas (`""` names none). The canvas's subtree
/// goes with it.
async fn unmake(kernel: &Arc<Kernel>, cab_id: &str, canvas_id: &str) {
    if !cab_id.is_empty() && record(kernel, cab_id).is_some() {
        tree(kernel, json!({"type": "delete_agent", "id": cab_id})).await;
    }
    if !canvas_id.is_empty() && record(kernel, canvas_id).is_some() {
        tree(kernel, json!({"type": "delete_agent", "id": canvas_id})).await;
    }
}

// ─── reading the cabinets ────────────────────────────────────────

/// The manager's live cabinets' records, in creation order.
fn cabinets(kernel: &Kernel, manager: &str) -> Vec<Rec> {
    let children = kernel
        .agents
        .get(&AgentId::from(manager))
        .map(|a| a.child_ids())
        .unwrap_or_default();
    children
        .iter()
        .filter_map(|cid| record(kernel, cid.as_str()))
        .filter(|rec| rec.get(ROLE).and_then(Value::as_str) == Some(CABINET))
        .collect()
}

/// `agent_id` and every agent under it, depth first.
fn subtree(kernel: &Kernel, agent_id: &str) -> Vec<String> {
    let Some(node) = kernel
        .agents
        .get(&AgentId::from(agent_id))
        .map(|a| Arc::clone(&a))
    else {
        return Vec::new();
    };
    let mut out = vec![agent_id.to_string()];
    for child in node.child_ids() {
        out.extend(subtree(kernel, child.as_str()));
    }
    out
}

/// The children of `node`, in tree order.
fn children(kernel: &Kernel, node: &str) -> Vec<String> {
    kernel
        .agents
        .get(&AgentId::from(node))
        .map(|a| a.child_ids().into_iter().map(|c| c.0).collect())
        .unwrap_or_default()
}

/// The `kernel_state` handler: a canvas's page loader is one of its children (the host's
/// `<canvas>_loader`).
const LOADER_HANDLER: &str = "kernel_state.tools";

/// Whether `child` is `node`'s page loader: the page's own records, not equipment and never
/// a result. Pruning keeps it, so a kept result's canvas stays openable.
fn is_loader(kernel: &Kernel, node: &str, child: &str) -> bool {
    child == format!("{node}_loader")
        || record(kernel, child).is_some_and(|r| {
            r.get("handler_module").and_then(Value::as_str) == Some(LOADER_HANDLER)
        })
}

/// Every agent on `canvas` at any depth (equipment and nested canvases), without the canvas
/// itself and without any page loader.
fn canvas_items(kernel: &Kernel, canvas: &str) -> Vec<String> {
    let mut out = Vec::new();
    for child in children(kernel, canvas) {
        if is_loader(kernel, canvas, &child) {
            continue;
        }
        out.push(child.clone());
        out.extend(canvas_items(kernel, &child));
    }
    out
}

/// The ids placed on a cabinet's canvas: its children (equipment, nested canvases).
fn equipment(kernel: &Kernel, canvas: &str) -> Vec<String> {
    children(kernel, canvas)
        .into_iter()
        .filter(|c| !is_loader(kernel, canvas, c))
        .collect()
}

fn is_ai(rec: &Rec) -> bool {
    rec.get("memory_store_id").is_some_and(Value::is_string)
        && rec.get("handler_module").is_some_and(Value::is_string)
}

/// A cabinet as one peer in a directory: its id, worker and task.
fn peer_of(rec: &Rec) -> Value {
    json!({
        "cabinet": rec.get("id").cloned().unwrap_or(Value::Null),
        "worker": rec.get("worker").cloned().unwrap_or(Value::Null),
        "task": rec.get("task").cloned().unwrap_or(Value::Null),
    })
}

/// The worker's post-system block: its role, its directory and how to work. It rides
/// AFTER the prompt in force (`post_system_prompt`), so the backend's own prompt is
/// never replaced.
fn instructions(cab: &Rec, peers: &[Value], master: &str) -> String {
    let worker = str_of(cab, "worker");
    let task = str_of(cab, "task");
    let id = str_of(cab, "id");
    let canvas = str_of(cab, "canvas");
    let mut lines: Vec<String> = vec![
        format!("You are the worker AI of cabinet `{id}`. Your task: {task}"),
        format!(
            "Your worker id is `{worker}`. You are one of {} cooperating workers, one per cabinet. Your peers:",
            peers.len() + 1
        ),
    ];
    if peers.is_empty() {
        lines.push("- none yet; more cabinets may open later.".to_string());
    } else {
        for p in peers {
            lines.push(format!(
                "- worker `{}` (cabinet `{}`): {}",
                p.get("worker").and_then(Value::as_str).unwrap_or(""),
                p.get("cabinet").and_then(Value::as_str).unwrap_or(""),
                p.get("task").and_then(Value::as_str).unwrap_or(""),
            ));
        }
    }
    lines.push(String::new());
    lines.push("How to work with each other:".to_string());
    lines.push(format!(
        "- Message a peer, or your master `{master}`, by sending `send` with a `text`: \
         `send {{target: <worker id>, payload: {{type: \"send\", text: \"...\"}}}}`. \
         They can message you the same way; messages are all you may send them."
    ));
    lines.push(format!(
        "- Report your state up to your cabinet, every time it changes: \
         `send {{target: \"{id}\", payload: {{type: \"status\", line: \"...\", \
         progress: 0-100, result: ...}}}}`. Put your final answer in `result`."
    ));
    lines.push(format!(
        "- Equipment: your cabinet is the canvas `{canvas}`. Build on it with `create` \
         (`send {{target: \"{canvas}\", payload: {{type: \"create\", record: {{handler_module: \"...\"}}}}}}`) \
         and show views with `place`. Those are the only verbs you hold on it."
    ));
    lines.join("\n")
}

/// Rewrite the directory and instructions of every live worker, from the live cabinets.
async fn refresh_directories(kernel: &Arc<Kernel>, manager: &str) -> Option<String> {
    let live = cabinets(kernel, manager);
    for cab in &live {
        let peers: Vec<Value> = live
            .iter()
            .filter(|c| str_of(c, "id") != str_of(cab, "id"))
            .map(peer_of)
            .collect();
        let text = instructions(cab, &peers, str_of(cab, "master"));
        if let Some(err) = wire(
            kernel,
            str_of(cab, "worker"),
            &[],
            &[],
            &[
                ("cabinet_peers", Value::Array(peers)),
                ("post_system_prompt", json!(text)),
            ],
        )
        .await
        {
            return Some(err);
        }
    }
    None
}

// ─── verbs: the manager ──────────────────────────────────────────

/// args: task:str (req), name:str?, backend:str? (an existing AI agent id; default the
/// master), setup:dict? (fields over the backend's copy; a new backend is flagged).
/// Opens one cabinet: a canvas of its own, a child of the canvas that spawns it (the
/// manager's `canvas_id`), with its worker AI, wired to every live worker and to the
/// master. The worker is NOT started — send it its first message (the task). Returns
/// {id, canvas, worker, master, backend, new_backend, peers}.
async fn open(manager: &str, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let task = match payload.get("task").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            return missing_hint(
                "open_cabinet",
                "task",
                "say what the cabinet must do: open_cabinet {task: \"...\"}",
            )
        }
    };
    let name = match payload.get("name").filter(|v| !v.is_null()) {
        None => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(_) => return bad("open_cabinet", "name", "a non-empty string"),
    };
    let Some(master) = current_origin().map(|a| a.0) else {
        let mut r = refusal_hint(
            MISSING_ARG,
            "open_cabinet: the master is the AI that sends it — an operator send has no master to wire to",
            "open the cabinet from the master AI's own turn",
        );
        with(&mut r, "arg", json!("master"));
        return r;
    };
    let canvas_id = match record(kernel, manager).and_then(|m| m.get("canvas_id").cloned()) {
        Some(Value::String(s)) if !s.is_empty() => s,
        _ => {
            let mut r = refusal_hint(
                MISSING_ARG,
                "open_cabinet: the manager names no canvas — its cabinets are canvases made by the canvas that spawns them",
                format!("name the spawning canvas on the manager: update_agent {{id: {manager}, canvas_id: <canvas node>}}"),
            );
            with(&mut r, "arg", json!("canvas_id"));
            return r;
        }
    };
    if record(kernel, &canvas_id).is_none() {
        let mut r = refusal(
            UNKNOWN_AGENT,
            format!("open_cabinet: canvas `{canvas_id}` does not exist"),
        );
        with(&mut r, "id", json!(canvas_id));
        return r;
    }
    let backend_id = match payload.get("backend").filter(|v| py_truthy(v)) {
        None => master.clone(),
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => return bad("open_cabinet", "backend", "an existing AI agent id"),
    };
    let Some(backend) = record(kernel, &backend_id) else {
        let mut r = refusal(
            UNKNOWN_AGENT,
            format!("open_cabinet: backend `{backend_id}` does not exist"),
        );
        with(&mut r, "id", json!(backend_id));
        return r;
    };
    if !is_ai(&backend) {
        let mut r = refusal(
            BAD_ARG,
            format!("open_cabinet: `{backend_id}` is not an AI backend (no memory_store_id)"),
        );
        with(&mut r, "arg", json!("backend"));
        return r;
    }
    let setup = match payload.get("setup").filter(|v| py_truthy(v)) {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return bad("open_cabinet", "setup", "a map of record fields"),
    };
    let mut wiring: Vec<&str> = setup
        .keys()
        .map(String::as_str)
        .filter(|k| is_wiring(k) || RESERVED_RECORD_KEYS.contains(k))
        .collect();
    if !wiring.is_empty() {
        wiring.sort_unstable();
        return bad(
            "open_cabinet",
            "setup",
            &format!("a map without wiring keys (got {})", py_list(&wiring)),
        );
    }
    if let Some(ceiling) = record(kernel, manager)
        .and_then(|m| m.get("max_cabinets").cloned())
        .filter(|v| !v.is_null())
    {
        let Some(limit) = ceiling.as_i64().filter(|n| *n >= 1) else {
            return bad(
                "open_cabinet",
                "max_cabinets",
                "a positive int on the manager record",
            );
        };
        let open_now = cabinets(kernel, manager).len();
        if open_now as i64 >= limit {
            return json!({
                "error": format!("open_cabinet: the ceiling is {limit} cabinets and {open_now} are open — close or conclude one first"),
                "reason": "ceiling",
                "ceiling": limit,
                "open": open_now,
            });
        }
    }

    let mut worker_rec: Map<String, Value> = backend
        .iter()
        .filter(|(k, _)| !NOT_COPIED.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    worker_rec.extend(setup);
    let store = worker_rec
        .get("memory_store_id")
        .cloned()
        .unwrap_or(Value::Null);
    if !store.as_str().is_some_and(|s| record(kernel, s).is_some()) {
        let mut r = refusal(
            UNKNOWN_AGENT,
            format!(
                "open_cabinet: the worker's memory store `{}` does not exist",
                py_repr_value(&store)
            ),
        );
        with(&mut r, "id", store.clone());
        return r;
    }
    let cred = worker_rec
        .get("credentials")
        .and_then(Value::as_object)
        .cloned();
    if let Some(cred) = &cred {
        let cred_store = cred.get("store").cloned().unwrap_or(Value::Null);
        if !cred_store
            .as_str()
            .is_some_and(|s| record(kernel, s).is_some())
        {
            let mut r = refusal(
                UNKNOWN_AGENT,
                format!(
                    "open_cabinet: the worker's credential store `{}` does not exist",
                    py_repr_value(&cred_store)
                ),
            );
            with(&mut r, "id", cred_store);
            return r;
        }
    }
    let new_backend = ["handler_module", "endpoint", "credentials"]
        .iter()
        .any(|k| {
            worker_rec.get(*k).unwrap_or(&Value::Null) != backend.get(*k).unwrap_or(&Value::Null)
        });

    let slug = name.as_deref().map(slug_of).unwrap_or_default();
    let cab_id = if slug.is_empty() {
        format!("cabinet_{}", token_hex(3))
    } else {
        format!("cabinet_{slug}")
    };
    let worker_id = format!("{cab_id}_worker");
    if record(kernel, &cab_id).is_some() {
        let mut r = refusal(
            "exists",
            format!("open_cabinet: `{cab_id}` already exists — name the cabinet another name"),
        );
        with(&mut r, "id", json!(cab_id));
        return r;
    }
    let display = name.clone().unwrap_or_else(|| head(&task, 60));

    // the manager may ask the spawning canvas for a child canvas, and nothing else
    let grant_err = wire(
        kernel,
        manager,
        &grants(&[(canvas_id.as_str(), json!([NEW_CANVAS_VERB]))]),
        &[],
        &[],
    )
    .await
    .or(wire(
        kernel,
        &canvas_id,
        &[],
        &grants(&[(manager, json!([NEW_CANVAS_VERB]))]),
        &[],
    )
    .await);
    if let Some(err) = grant_err {
        return json!({"error": format!("open_cabinet: could not grant the canvas: {err}")});
    }
    // the canvas the host will make for `new_canvas {name: cab_id}`: `canvas_<name>`
    let cab_canvas = format!("canvas_{cab_id}");
    let made = tree(
        kernel,
        json!({
            "type": "create_agent",
            "parent_id": manager,
            "id": cab_id,
            "handler_module": HANDLER_MODULE,
            ROLE: CABINET,
            "task": task,
            "master": master,
            "worker": worker_id,
            "canvas": cab_canvas,
            "status": {"line": "", "progress": null, "result": null, "at": null},
            "display_name": display,
            "description": format!("Cabinet: {}", head(&task, 200)),
            // the live status cell on the main canvas: it opens the cabinet's canvas
            "view": {
                "kind": "html",
                "bytes": VIEW_HTML.replace("__CABINET_ID__", &cab_id),
                "calls": ["view", "watch"],
            },
            "can_call": {"$self": "*"},
            "callable_by": {
                "$self": "*",
                "$parent": "*",
                worker_id.clone(): [STATUS_VERB],
                // the master reads its cabinet directly; the canvas (`user`) draws its panel
                master.clone(): ["view", "reflect"],
                "user": ["view", "watch", "reflect"],
            },
        }),
    )
    .await;
    if error_of(&made).is_some() {
        return made;
    }

    // THE CABINET'S CANVAS: the spawning canvas makes it, a child of its own. The record
    // above already names it, so the host's reach knows it for a cabinet's canvas the
    // moment it exists (never a holder with the reach of every AI).
    let made_canvas = call(
        kernel,
        &canvas_id,
        json!({"type": NEW_CANVAS_VERB, "name": cab_id}),
    )
    .await;
    if let Some(err) = error_of(&made_canvas) {
        unmake(kernel, &cab_id, "").await;
        let mut out = made_canvas.clone();
        with(
            &mut out,
            "error",
            json!(format!(
                "open_cabinet: the canvas `{canvas_id}` did not make the cabinet's canvas: {err}"
            )),
        );
        return out;
    }
    match made_canvas.get("id").and_then(Value::as_str) {
        Some(c) if c == cab_canvas => {}
        other => {
            unmake(kernel, &cab_id, other.unwrap_or("")).await;
            return json!({
                "error": format!(
                    "open_cabinet: the canvas `{canvas_id}` answered {} for the cabinet's canvas, not `{cab_canvas}`",
                    other.map_or_else(|| "no id".to_string(), |c| format!("`{c}`"))
                )
            });
        }
    }

    let handler = worker_rec.remove("handler_module").unwrap_or(Value::Null);
    let mut worker_payload = Map::new();
    worker_payload.insert("type".into(), json!("create_agent"));
    worker_payload.insert("parent_id".into(), json!(cab_id));
    worker_payload.insert("id".into(), json!(worker_id));
    worker_payload.insert("handler_module".into(), handler);
    worker_payload.insert(
        "description".into(),
        json!(format!("Worker of cabinet {cab_id}: {}", head(&task, 200))),
    );
    // itself, its cabinet's `status` up, and its cabinet canvas (`create` and `place`
    // on it, nothing else). Nothing on the root.
    worker_payload.insert(
        "can_call".into(),
        json!({
            "$self": "*",
            "$parent": [STATUS_VERB],
            cab_canvas.clone(): CANVAS_VERBS,
        }),
    );
    worker_payload.insert(
        "callable_by".into(),
        json!({"$self": "*", manager: "*", master.clone(): "*"}),
    );
    worker_payload.extend(worker_rec.clone());
    let made_worker = tree(kernel, Value::Object(worker_payload)).await;
    if error_of(&made_worker).is_some() {
        unmake(kernel, &cab_id, &cab_canvas).await;
        return made_worker;
    }

    // Wire the NEW worker to its canvas, the master, the stores and every live worker,
    // and every live worker to it. Every step runs (no short-circuit); the first failure
    // deletes the cabinet it half-made.
    let mut failure: Option<String> = None;
    let mut step = |err: Option<String>| {
        if failure.is_none() {
            failure = err;
        }
    };
    // the canvas makes the equipment its worker `create`s: it may ask the root for
    // `create_agent`, and for nothing else (the root names it back)
    let root = root_id(kernel);
    step(
        wire(
            kernel,
            &cab_canvas,
            &grants(&[(root.as_str(), json!([CREATE_VERB]))]),
            &grants(&[(worker_id.as_str(), json!(CANVAS_VERBS))]),
            &[],
        )
        .await,
    );
    step(
        wire(
            kernel,
            &root,
            &[],
            &grants(&[(cab_canvas.as_str(), json!([CREATE_VERB]))]),
            &[],
        )
        .await,
    );
    step(
        wire(
            kernel,
            &master,
            &grants(&[
                (worker_id.as_str(), json!("*")),
                (cab_id.as_str(), json!(["view", "reflect"])),
            ]),
            &grants(&[(worker_id.as_str(), json!(MESSAGE_VERBS))]),
            &[],
        )
        .await,
    );
    step(
        wire(
            kernel,
            manager,
            &grants(&[(worker_id.as_str(), json!("*"))]),
            &[],
            &[],
        )
        .await,
    );
    let store_id = store.as_str().unwrap_or("").to_string();
    step(
        wire(
            kernel,
            &store_id,
            &[],
            &grants(&[(worker_id.as_str(), json!("*"))]),
            &[],
        )
        .await,
    );
    step(
        wire(
            kernel,
            &worker_id,
            &grants(&[
                (master.as_str(), json!(MESSAGE_VERBS)),
                (store_id.as_str(), json!("*")),
            ]),
            &[],
            &[],
        )
        .await,
    );
    if let Some(cred) = &cred {
        let cred_store = cred
            .get("store")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        step(
            wire(
                kernel,
                &cred_store,
                &[],
                &grants(&[(worker_id.as_str(), json!("*"))]),
                &[],
            )
            .await,
        );
        step(
            wire(
                kernel,
                &worker_id,
                &grants(&[(cred_store.as_str(), json!("*"))]),
                &[],
                &[],
            )
            .await,
        );
    }
    for other in cabinets(kernel, manager) {
        let other_worker = str_of(&other, "worker").to_string();
        if other_worker == worker_id {
            continue;
        }
        step(
            wire(
                kernel,
                &other_worker,
                &grants(&[(worker_id.as_str(), json!(MESSAGE_VERBS))]),
                &grants(&[(worker_id.as_str(), json!(MESSAGE_VERBS))]),
                &[],
            )
            .await,
        );
        step(
            wire(
                kernel,
                &worker_id,
                &grants(&[(other_worker.as_str(), json!(MESSAGE_VERBS))]),
                &grants(&[(other_worker.as_str(), json!(MESSAGE_VERBS))]),
                &[],
            )
            .await,
        );
    }
    if failure.is_none() {
        failure = refresh_directories(kernel, manager).await;
    }
    if let Some(failure) = failure {
        unmake(kernel, &cab_id, &cab_canvas).await;
        return json!({
            "error": format!("open_cabinet: wiring failed and the cabinet was removed: {failure}")
        });
    }

    if record(kernel, &cab_id).is_none() {
        return json!({"error": "open_cabinet: the cabinet vanished while wiring"});
    }
    let peers: Vec<Value> = cabinets(kernel, manager)
        .iter()
        .filter(|c| str_of(c, "id") != cab_id)
        .map(peer_of)
        .collect();
    let mut out = json!({
        "id": cab_id,
        "canvas": cab_canvas,
        "worker": worker_id,
        "master": master,
        "backend": backend_id,
        "new_backend": new_backend,
        "peers": peers,
        "next": format!(
            "send `{worker_id}` its first message (the task) with {{type: \"send\", text: ...}}; \
             show its status cell on the spawning canvas with that canvas view's place {{backend_id: \"{cab_id}\", x, y}}"
        ),
    });
    if new_backend {
        with(&mut out, "warning", json!(NEW_BACKEND_WARNING));
    }
    out
}

/// Every agent this cabinet holds that is not kept and holds no kept agent is deleted
/// (python `_prune`): the kept agents stay with their path up to `node`. A page loader is
/// never deleted here (it is the canvas's own records). Returns the
/// deleted ids and the error, if one stopped it.
async fn prune(kernel: &Arc<Kernel>, node: &str, keep: &[String]) -> (Vec<String>, Option<String>) {
    let mut deleted = Vec::new();
    for child in children(kernel, node) {
        if keep.contains(&child) || is_loader(kernel, node, &child) {
            continue;
        }
        let sub = subtree(kernel, &child);
        if sub.iter().any(|s| keep.contains(s)) {
            let (more, err) = Box::pin(prune(kernel, &child, keep)).await;
            deleted.extend(more);
            if err.is_some() {
                return (deleted, err);
            }
            continue;
        }
        let res = tree(kernel, json!({"type": "delete_agent", "id": child})).await;
        if !deleted_ok(&res) {
            let err = error_of(&res).unwrap_or_else(|| format!("`{child}` was not deleted"));
            return (deleted, Some(err));
        }
        deleted.extend(sub);
    }
    (deleted, None)
}

/// args: id:str (req) — a cabinet of this manager. Deletes the cabinet with its worker,
/// and its canvas with ALL its equipment, then rewrites the live workers' directories.
/// `keep` names results on this canvas to keep (from `conclude`): the canvas stays with
/// them and everything else on it goes. Returns {closed, deleted, kept}.
async fn close(manager: &str, payload: &Value, kernel: &Arc<Kernel>, keep: &[String]) -> Value {
    let cab_id = match payload.get("id").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return missing_hint(
                "close_cabinet",
                "id",
                "close_cabinet {id: \"cabinet_<name>\"}",
            )
        }
    };
    let owned = record(kernel, &cab_id).is_some_and(|rec| {
        rec.get(ROLE).and_then(Value::as_str) == Some(CABINET)
            && rec.get("parent_id").and_then(Value::as_str) == Some(manager)
    });
    if !owned {
        let mut r = refusal(
            UNKNOWN_AGENT,
            format!("close_cabinet: `{cab_id}` is not a cabinet of this manager"),
        );
        with(&mut r, "id", json!(cab_id));
        return r;
    }
    let rec = record(kernel, &cab_id).unwrap_or_default();
    let canvas = str_of(&rec, "canvas").to_string();
    let has_canvas = !canvas.is_empty() && record(kernel, &canvas).is_some();
    let kept_here: Vec<String> = if has_canvas {
        let below = canvas_items(kernel, &canvas);
        keep.iter().filter(|k| below.contains(k)).cloned().collect()
    } else {
        Vec::new()
    };
    let mut gone = subtree(kernel, &cab_id);
    let res = tree(kernel, json!({"type": "delete_agent", "id": cab_id})).await;
    if !deleted_ok(&res) {
        let mut out = res.clone();
        let error =
            error_of(&res).unwrap_or_else(|| format!("close_cabinet: `{cab_id}` was not deleted"));
        with(&mut out, "error", json!(error));
        return out;
    }
    if has_canvas && !kept_here.is_empty() {
        let (pruned, err) = prune(kernel, &canvas, keep).await;
        gone.extend(pruned);
        if let Some(err) = err {
            return json!({
                "error": format!("close_cabinet: `{cab_id}` was deleted, but its canvas `{canvas}` was not pruned: {err}"),
                "deleted": gone,
            });
        }
    } else if has_canvas {
        gone.extend(subtree(kernel, &canvas));
        let res = tree(kernel, json!({"type": "delete_agent", "id": canvas})).await;
        if !deleted_ok(&res) {
            let mut out = res.clone();
            let error = error_of(&res).unwrap_or_else(|| {
                format!("close_cabinet: `{cab_id}` was deleted, but its canvas `{canvas}` was not")
            });
            with(&mut out, "error", json!(error));
            with(&mut out, "deleted", json!(gone));
            return out;
        }
    }
    if let Some(err) = refresh_directories(kernel, manager).await {
        return json!({
            "error": format!("close_cabinet: deleted `{cab_id}`, but a worker's directory did not update: {err}"),
            "deleted": gone,
        });
    }
    let mut kept_sorted = kept_here;
    kept_sorted.sort();
    json!({"closed": cab_id, "deleted": gone, "kept": kept_sorted})
}

/// No args. Every live cabinet: its task, worker, canvas, live status and equipment.
/// The master's monitor.
fn cabinets_verb(manager: &str, kernel: &Kernel) -> Value {
    let rows: Vec<Value> = cabinets(kernel, manager)
        .iter()
        .map(|c| {
            json!({
                "id": c.get("id").cloned().unwrap_or(Value::Null),
                "name": c.get("display_name").cloned().unwrap_or(Value::Null),
                "task": c.get("task").cloned().unwrap_or(Value::Null),
                "worker": c.get("worker").cloned().unwrap_or(Value::Null),
                "canvas": c.get("canvas").cloned().unwrap_or(Value::Null),
                "status": c.get("status").cloned().unwrap_or(Value::Null),
                "equipment": equipment(kernel, str_of(c, "canvas")),
            })
        })
        .collect();
    let count = rows.len();
    json!({
        "cabinets": rows,
        "count": count,
        "origin": format!("{manager} cabinets — re-send; changes as cabinets open, report and close"),
    })
}

/// args: keep:[str]? — the ids to KEEP: live cabinets of this manager (kept whole), or
/// results on a cabinet's canvas (the canvas and the path to them stay). Every other
/// cabinet is closed. A keep id that is neither is refused before anything is deleted.
/// The manager stays. Returns {concluded, kept, deleted, count}.
async fn conclude(manager: &str, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let keep: Vec<String> = match payload.get("keep") {
        None => Vec::new(),
        Some(Value::Array(items))
            if items
                .iter()
                .all(|k| k.as_str().is_some_and(|s| !s.is_empty())) =>
        {
            items
                .iter()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect()
        }
        Some(_) => return bad("conclude", "keep", "a list of cabinet or result ids"),
    };
    let live = cabinets(kernel, manager);
    let cab_ids: BTreeSet<String> = live.iter().map(|c| str_of(c, "id").to_string()).collect();
    let results: BTreeSet<String> = live
        .iter()
        .filter(|c| !str_of(c, "canvas").is_empty())
        .flat_map(|c| canvas_items(kernel, str_of(c, "canvas")))
        .collect();
    let unknown: Vec<&str> = keep
        .iter()
        .map(String::as_str)
        .filter(|k| !cab_ids.contains(*k) && !results.contains(*k))
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .collect();
    if !unknown.is_empty() {
        return json!({
            "error": format!("conclude: keep names no live cabinet or result of this manager: {}", py_list(&unknown)),
            "reason": "unknown_keep",
            "unknown": unknown,
        });
    }
    let mut kept_sorted = keep.clone();
    kept_sorted.sort();
    let mut concluded: Vec<Value> = Vec::new();
    let mut deleted: Vec<Value> = Vec::new();
    for cab in &live {
        let cab_id = str_of(cab, "id").to_string();
        if keep.contains(&cab_id) {
            continue;
        }
        let res = close(manager, &json!({"id": cab_id}), kernel, &keep).await;
        if error_of(&res).is_some() {
            let mut out = res.clone();
            with(&mut out, "concluded", Value::Array(concluded));
            with(&mut out, "kept", json!(kept_sorted));
            with(&mut out, "deleted", Value::Array(deleted));
            return out;
        }
        concluded.push(json!(cab_id));
        if let Some(Value::Array(d)) = res.get("deleted") {
            deleted.extend(d.iter().cloned());
        }
    }
    let count = concluded.len();
    json!({"concluded": concluded, "kept": kept_sorted, "deleted": deleted, "count": count})
}

// ─── verbs: a cabinet ────────────────────────────────────────────

/// args: line:str (req), progress:number 0-100?, result:any?. The worker's report up:
/// stored on the cabinet and emitted on its own id, so its view and every watcher see
/// it live.
async fn status(cab_id: &str, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(line) = payload.get("line").and_then(Value::as_str) else {
        return bad(
            STATUS_VERB,
            "line",
            "a string: what the worker is doing now",
        );
    };
    let progress = payload.get("progress").cloned().unwrap_or(Value::Null);
    if !progress.is_null() {
        let in_range = match &progress {
            Value::Number(n) => n.as_f64().is_some_and(|f| (0.0..=100.0).contains(&f)),
            _ => false,
        };
        if !in_range {
            return bad(STATUS_VERB, "progress", "a number from 0 to 100");
        }
    }
    let st = json!({
        "line": line,
        "progress": progress,
        "result": payload.get("result").cloned().unwrap_or(Value::Null),
        "at": now_secs(),
    });
    let mut patch = Map::new();
    patch.insert("status".into(), st.clone());
    kernel.update(&AgentId::from(cab_id), patch);
    kernel
        .emit(
            &AgentId::from(cab_id),
            json!({"type": "update", "cabinet": cab_id, "status": st}),
        )
        .await;
    json!({"ok": true, "cabinet": cab_id})
}

/// No args. The cabinet's panel: its task, worker, canvas, live status and equipment.
fn view(cab_id: &str, kernel: &Kernel) -> Value {
    let rec = record(kernel, cab_id).unwrap_or_default();
    json!({
        "id": cab_id,
        "task": rec.get("task").cloned().unwrap_or(Value::Null),
        "worker": rec.get("worker").cloned().unwrap_or(Value::Null),
        "canvas": rec.get("canvas").cloned().unwrap_or(Value::Null),
        "status": rec.get("status").cloned().unwrap_or(Value::Null),
        "equipment": equipment(kernel, str_of(&rec, "canvas")),
    })
}

// ─── reflect and dispatch ────────────────────────────────────────

/// No args. The agent's role and what it holds.
fn reflect(id: &str, kernel: &Kernel) -> Value {
    let rec = record(kernel, id).unwrap_or_default();
    if rec.get(ROLE).and_then(Value::as_str) == Some(MANAGER) {
        return json!({
            "sentence": "A cabinet manager: opens cabinets (a canvas of its own, a child of the spawning canvas, with one worker AI each), monitors them, closes them, concludes.",
            "role": MANAGER,
            "canvas_id": rec.get("canvas_id").cloned().unwrap_or(Value::Null),
            "max_cabinets": rec.get("max_cabinets").cloned().unwrap_or(Value::Null),
            "open": cabinets(kernel, id).len(),
        });
    }
    let task = str_of(&rec, "task");
    let sentence = format!("A cabinet: the status cell of one worker AI and its canvas — {task}");
    json!({
        "sentence": sentence.trim_end(),
        "role": CABINET,
        "task": rec.get("task").cloned().unwrap_or(Value::Null),
        "worker": rec.get("worker").cloned().unwrap_or(Value::Null),
        "canvas": rec.get("canvas").cloned().unwrap_or(Value::Null),
        "status": rec.get("status").cloned().unwrap_or(Value::Null),
    })
}

/// The docs of each verb, python's VERBS docstrings.
fn verb_docs_json() -> Value {
    json!({
        "reflect": "No args. The agent's role and what it holds.",
        "open_cabinet": "args: task:str (req), name:str?, backend:str? (an existing AI agent id; default the master), setup:dict? (fields over the backend's copy; a new backend is flagged). Opens one cabinet: a canvas of its own, a child of the canvas that spawns it (the manager's `canvas_id`), with its worker AI, wired to every live worker and to the master. The worker is NOT started — send it its first message (the task). Returns {id, canvas, worker, master, backend, new_backend, peers}.",
        "close_cabinet": "args: id:str (req) — a cabinet of this manager. Deletes the cabinet with its worker, and its canvas with ALL the equipment on it, then rewrites the live workers' directories. Returns {closed, deleted, kept}.",
        "cabinets": "No args. Every live cabinet: its task, worker, canvas, live status and equipment. The master's monitor.",
        "conclude": "args: keep:[str]? — the ids to KEEP: live cabinets of this manager (kept whole), or results on a cabinet's canvas (the canvas and the path to them stay). Every other cabinet is closed: its canvas, worker and equipment deleted. A keep id that is neither is refused before anything is deleted. The manager stays. Returns {concluded, kept, deleted, count}.",
        "status": "args: line:str (req), progress:number 0-100?, result:any?. The worker's report up: stored on the cabinet and emitted on its own id, so its view and every watcher see it live.",
        "view": "No args. The cabinet's panel: its task, worker, canvas, live status and equipment.",
    })
}

/// The cabinet bundle.
#[derive(Default)]
pub struct CabinetBundle;

#[async_trait]
impl Bundle for CabinetBundle {
    /// The kind, as the catalog lists it (`<kind>.tools` is its handler module).
    fn name(&self) -> &str {
        "cabinet"
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let id = agent_id.as_str();
        let t = payload.get("type").cloned().unwrap_or(Value::Null);
        let verb = t.as_str().unwrap_or("");
        let role = record(kernel, id)
            .and_then(|r| r.get(ROLE).and_then(Value::as_str).map(str::to_string));
        // a serial verb of a manager holds the manager's lock for its whole run
        let _held = match role.as_deref() {
            Some(MANAGER) if SERIAL_VERBS.contains(&verb) => Some(manager_lock(id).await),
            _ => None,
        };
        let reply = match role.as_deref() {
            Some(MANAGER) => match verb {
                "reflect" => reflect(id, kernel),
                "open_cabinet" => open(id, payload, kernel).await,
                "close_cabinet" => close(id, payload, kernel, &[]).await,
                "cabinets" => cabinets_verb(id, kernel),
                "conclude" => conclude(id, payload, kernel).await,
                _ => unknown_verb(&t, MANAGER),
            },
            Some(CABINET) => match verb {
                "reflect" => reflect(id, kernel),
                "status" => status(id, payload, kernel).await,
                "view" => view(id, kernel),
                _ => unknown_verb(&t, CABINET),
            },
            _ => {
                let mut r = refusal(
                    UNHANDLED,
                    format!("cabinet: {id} has no cabinet_role (manager or cabinet)"),
                );
                with(&mut r, "verb", t.clone());
                r
            }
        };
        Ok(Some(reply))
    }

    fn verb_docs(&self) -> Value {
        verb_docs_json()
    }
}

fn unknown_verb(t: &Value, role: &str) -> Value {
    let mut r = refusal(
        UNHANDLED,
        format!("cabinet: unknown type {} for a {role}", py_repr_value(t)),
    );
    with(&mut r, "verb", t.clone());
    r
}

#[cfg(test)]
mod tests;
