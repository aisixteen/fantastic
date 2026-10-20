//! Cabinets — a master AI opens cabinets: subcanvases, each with one worker AI.
//!
//! Port of python `cabinet/tools.py` (the reference runtime): verb for verb, with
//! the same refusals, the same grants and the same wiring. See `readme.md`.
//!
//! THE SHAPE. One MANAGER (a `cabinet.tools` agent the operator makes once) owns the
//! CABINETS as its children. A cabinet is a subcanvas: its record carries a live html
//! view (`view.html`) that watches it, and it owns ONE WORKER AI as its child. The
//! worker's EQUIPMENT is the worker's own child, reached through its `$subtree`
//! grant and nothing else.
//!
//! NOTHING IS MADE OR CHANGED BEHIND A GRANT. The manager makes and edits every record
//! by a gated send to the ROOT (`create_agent`, `update_agent`, `delete_agent`), as
//! itself. The operator grants the manager those verbs on the root. The cabinet makes
//! its worker's equipment on the root (`create_agent`, granted by the manager at open),
//! forcing the parent to the worker; the worker holds nothing on the root.
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
use std::collections::HashMap;
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

/// The worker's equipment, placed under it: the one other verb a worker sends its cabinet.
const EQUIP_VERB: &str = "equip";

/// The manager's verbs that wire or unwire: each runs under the manager's lock.
const SERIAL_VERBS: [&str; 3] = ["open_cabinet", "close_cabinet", "conclude"];

/// The grant keys an equipment agent may name without naming an agent: itself, its
/// parent (the worker) and its own subtree. Any other key must be an agent already
/// under the worker.
const RELATIVE_KEYS: [&str; 3] = ["$self", "$parent", "$subtree"];

/// One lock per manager id (python `_LOCKS`, keyed by the manager): see [`manager_lock`].
/// An entry leaves the table when its last holder lets go, so a deleted manager leaves
/// nothing behind.
static MANAGER_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// What one worker may send another worker, or the master: messages, and nothing else.
const MESSAGE_VERBS: [&str; 2] = ["send", "reflect"];

/// Record keys a worker does NOT copy from its backend (python `_NOT_COPIED`).
const NOT_COPIED: [&str; 24] = [
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
    "status",
    "cabinet_peers",
    "max_cabinets",
];

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

/// Edit `holder`'s grants and fields in ONE update. Each `(key, verbs)` pair sets one
/// grant key; the whole maps are restated, because `update_agent` replaces them.
/// Returns the refusal's error, or `None`.
async fn wire(
    kernel: &Arc<Kernel>,
    holder: &str,
    can_call: &[(String, Value)],
    callable_by: &[(String, Value)],
    fields: &[(&str, Value)],
) -> Option<String> {
    let Some(rec) = record(kernel, holder) else {
        return Some(format!("`{holder}` does not exist"));
    };
    let mut upd = Map::new();
    upd.insert("type".into(), json!("update_agent"));
    upd.insert("id".into(), json!(holder));
    for (key, value) in fields {
        upd.insert((*key).to_string(), value.clone());
    }
    for (field, changes) in [("can_call", can_call), ("callable_by", callable_by)] {
        if changes.is_empty() {
            continue;
        }
        let mut grant = rec
            .get(field)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (key, verbs) in changes {
            grant.insert(key.clone(), verbs.clone());
        }
        upd.insert(field.to_string(), Value::Object(grant));
    }
    if upd.len() == 2 {
        return None;
    }
    error_of(&tree(kernel, Value::Object(upd)).await)
}

/// `(key, verbs)` pairs from literals.
fn grants(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
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

/// The ids of a worker's direct children: its equipment.
fn equipment(kernel: &Kernel, worker: &str) -> Vec<String> {
    kernel
        .agents
        .get(&AgentId::from(worker))
        .map(|a| a.child_ids().into_iter().map(|c| c.0).collect())
        .unwrap_or_default()
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
        "- Equipment: send `equip` to your cabinet `{id}` with `handler_module` \
         (and optionally `id`, `setup`, `can_call`, `callable_by`): \
         `send {{target: \"{id}\", payload: {{type: \"equip\", handler_module: \"...\"}}}}`. \
         The new agent is placed under you, inside this cabinet, and granted within it only. \
         A grant may name `$self`, `$parent`, `$subtree`, `{worker}` or an agent already \
         under you, nothing else."
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
/// Opens one cabinet: a subcanvas with its worker AI, wired to every live worker and to
/// the master. The worker is NOT started — send it its first message (the task).
/// Returns {id, worker, master, backend, new_backend, peers}.
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
    let root = root_id(kernel);
    let display = name.clone().unwrap_or_else(|| head(&task, 60));
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
            "status": {"line": "", "progress": null, "result": null, "at": null},
            "display_name": display,
            "description": format!("Cabinet: {}", head(&task, 200)),
            "view": {
                "kind": "html",
                "bytes": VIEW_HTML.replace("__CABINET_ID__", &cab_id),
                "calls": ["view", "watch"],
            },
            // the cabinet makes its worker's equipment on the root, under the worker only
            "can_call": {"$self": "*", root.clone(): ["create_agent"]},
            "callable_by": {
                "$self": "*",
                "$parent": "*",
                worker_id.clone(): [STATUS_VERB, EQUIP_VERB],
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
    // its equipment (`$subtree`), itself, and its cabinet's channels: `status` up, and
    // `equip` to place equipment under itself. Nothing on the root.
    worker_payload.insert(
        "can_call".into(),
        json!({
            "$self": "*",
            "$parent": [STATUS_VERB, EQUIP_VERB],
            "$subtree": "*",
        }),
    );
    worker_payload.insert(
        "callable_by".into(),
        json!({"$self": "*", manager: "*", master.clone(): "*"}),
    );
    worker_payload.extend(worker_rec.clone());
    let made_worker = tree(kernel, Value::Object(worker_payload)).await;
    if error_of(&made_worker).is_some() {
        tree(kernel, json!({"type": "delete_agent", "id": cab_id})).await;
        return made_worker;
    }

    // Wire the NEW worker to the master, the stores and every live worker, and every
    // live worker to it. Every step runs (no short-circuit); the first failure deletes
    // the cabinet it half-made.
    let mut failure: Option<String> = None;
    let mut step = |err: Option<String>| {
        if failure.is_none() {
            failure = err;
        }
    };
    // the cabinet (not the worker) is the one that creates on the root
    step(
        wire(
            kernel,
            &root,
            &[],
            &grants(&[(cab_id.as_str(), json!(["create_agent"]))]),
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
        tree(kernel, json!({"type": "delete_agent", "id": cab_id})).await;
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
        "worker": worker_id,
        "master": master,
        "backend": backend_id,
        "new_backend": new_backend,
        "peers": peers,
        "next": format!("send `{worker_id}` its first message (the task) with {{type: \"send\", text: ...}}"),
    });
    if new_backend {
        with(&mut out, "warning", json!(NEW_BACKEND_WARNING));
    }
    out
}

/// args: id:str (req) — a cabinet of this manager. Deletes the cabinet with its worker
/// and ALL its equipment, then rewrites the live workers' directories. Returns
/// {closed, deleted}.
async fn close(manager: &str, payload: &Value, kernel: &Arc<Kernel>) -> Value {
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
    let gone = subtree(kernel, &cab_id);
    let res = tree(kernel, json!({"type": "delete_agent", "id": cab_id})).await;
    if !res.get("deleted").is_some_and(py_truthy) {
        let mut out = res.clone();
        let error =
            error_of(&res).unwrap_or_else(|| format!("close_cabinet: `{cab_id}` was not deleted"));
        with(&mut out, "error", json!(error));
        return out;
    }
    if let Some(err) = refresh_directories(kernel, manager).await {
        return json!({
            "error": format!("close_cabinet: deleted `{cab_id}`, but a worker's directory did not update: {err}"),
            "deleted": gone,
        });
    }
    json!({"closed": cab_id, "deleted": gone})
}

/// No args. Every live cabinet: its task, worker, live status and equipment. The
/// master's monitor.
fn cabinets_verb(manager: &str, kernel: &Kernel) -> Value {
    let rows: Vec<Value> = cabinets(kernel, manager)
        .iter()
        .map(|c| {
            json!({
                "id": c.get("id").cloned().unwrap_or(Value::Null),
                "name": c.get("display_name").cloned().unwrap_or(Value::Null),
                "task": c.get("task").cloned().unwrap_or(Value::Null),
                "worker": c.get("worker").cloned().unwrap_or(Value::Null),
                "status": c.get("status").cloned().unwrap_or(Value::Null),
                "equipment": equipment(kernel, str_of(c, "worker")),
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

/// No args. Closes EVERY cabinet this manager opened, deleting each subcanvas with its
/// worker and equipment. The manager stays. Returns {concluded, deleted}.
async fn conclude(manager: &str, kernel: &Arc<Kernel>) -> Value {
    let mut concluded: Vec<Value> = Vec::new();
    let mut deleted: Vec<Value> = Vec::new();
    for cab in cabinets(kernel, manager) {
        let cab_id = str_of(&cab, "id").to_string();
        let res = close(manager, &json!({"id": cab_id}), kernel).await;
        if error_of(&res).is_some() {
            let mut out = res.clone();
            with(&mut out, "concluded", Value::Array(concluded));
            with(&mut out, "deleted", Value::Array(deleted));
            return out;
        }
        concluded.push(json!(cab_id));
        if let Some(Value::Array(d)) = res.get("deleted") {
            deleted.extend(d.iter().cloned());
        }
    }
    let count = concluded.len();
    json!({"concluded": concluded, "deleted": deleted, "count": count})
}

/// args: handler_module:str (req), id:str?, setup:dict? (record fields, no wiring keys), can_call:dict?, callable_by:dict? (keys: $self, $parent, $subtree, the worker or an agent already under it). Places ONE agent under this cabinet's worker, granted inside the cabinet only. Returns {id, parent, handler_module}.
async fn equip(cab_id: &str, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if payload.get("parent_id").is_some() {
        return bad(
            EQUIP_VERB,
            "parent_id",
            "absent: equipment is always placed under its worker",
        );
    }
    let rec = record(kernel, cab_id).unwrap_or_default();
    let worker = str_of(&rec, "worker").to_string();
    let handler_module = match payload.get("handler_module").and_then(Value::as_str) {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => {
            return missing_hint(
                EQUIP_VERB,
                "handler_module",
                "equip {handler_module: \"file_bridge.tools\", ...}",
            )
        }
    };
    let eq_id = match payload.get("id").filter(|v| !v.is_null()) {
        None => format!("{worker}_eq_{}", token_hex(3)),
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(_) => return bad(EQUIP_VERB, "id", "a non-empty string"),
    };
    let setup = match payload.get("setup").filter(|v| py_truthy(v)) {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return bad(EQUIP_VERB, "setup", "a map of record fields"),
    };
    let mut wiring: Vec<&str> = setup
        .keys()
        .map(String::as_str)
        .filter(|k| is_wiring(k) || RESERVED_RECORD_KEYS.contains(k) || *k == "handler_module")
        .collect();
    if !wiring.is_empty() {
        wiring.sort_unstable();
        return bad(
            EQUIP_VERB,
            "setup",
            &format!("a map without wiring keys (got {})", py_list(&wiring)),
        );
    }
    // the grants stay inside the cabinet: every key names the equipment's own relatives
    // or an agent already under the worker (the worker and its equipment, nothing outside)
    let inside = subtree(kernel, &worker);
    let mut grants = Map::new();
    for field in ["can_call", "callable_by"] {
        let Some(grant) = payload.get(field).filter(|v| !v.is_null()) else {
            continue;
        };
        let Value::Object(map) = grant else {
            return bad(EQUIP_VERB, field, "a grant map");
        };
        let mut outside: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|k| !RELATIVE_KEYS.contains(k) && !inside.iter().any(|i| i == k))
            .collect();
        if !outside.is_empty() {
            outside.sort_unstable();
            return bad(
                EQUIP_VERB,
                field,
                &format!(
                    "a grant inside this cabinet only (got {})",
                    py_list(&outside)
                ),
            );
        }
        grants.insert(field.to_string(), grant.clone());
    }
    // callable_by defaults to the worker and the equipment's own subtree: reached by its worker alone
    grants
        .entry("callable_by")
        .or_insert_with(|| json!({"$self": "*", "$parent": "*"}));
    let mut create = Map::new();
    create.insert("type".into(), json!("create_agent"));
    create.insert("parent_id".into(), json!(worker));
    create.insert("id".into(), json!(eq_id));
    create.insert("handler_module".into(), json!(handler_module));
    create.extend(setup);
    create.extend(grants);
    let made = tree(kernel, Value::Object(create)).await;
    if error_of(&made).is_some() {
        return made;
    }
    json!({"id": eq_id, "parent": worker, "handler_module": handler_module})
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

/// No args. The cabinet's panel: its task, worker, live status and equipment.
fn view(cab_id: &str, kernel: &Kernel) -> Value {
    let rec = record(kernel, cab_id).unwrap_or_default();
    json!({
        "id": cab_id,
        "task": rec.get("task").cloned().unwrap_or(Value::Null),
        "worker": rec.get("worker").cloned().unwrap_or(Value::Null),
        "status": rec.get("status").cloned().unwrap_or(Value::Null),
        "equipment": equipment(kernel, str_of(&rec, "worker")),
    })
}

// ─── reflect and dispatch ────────────────────────────────────────

/// No args. The agent's role and what it holds.
fn reflect(id: &str, kernel: &Kernel) -> Value {
    let rec = record(kernel, id).unwrap_or_default();
    if rec.get(ROLE).and_then(Value::as_str) == Some(MANAGER) {
        return json!({
            "sentence": "A cabinet manager: opens cabinets (a subcanvas with one worker AI each), monitors them, closes them, concludes.",
            "role": MANAGER,
            "max_cabinets": rec.get("max_cabinets").cloned().unwrap_or(Value::Null),
            "open": cabinets(kernel, id).len(),
        });
    }
    let task = str_of(&rec, "task");
    let sentence = format!("A cabinet: the subcanvas of one worker AI — {task}");
    json!({
        "sentence": sentence.trim_end(),
        "role": CABINET,
        "task": rec.get("task").cloned().unwrap_or(Value::Null),
        "worker": rec.get("worker").cloned().unwrap_or(Value::Null),
        "status": rec.get("status").cloned().unwrap_or(Value::Null),
    })
}

/// The docs of each verb, python's VERBS docstrings.
fn verb_docs_json() -> Value {
    json!({
        "reflect": "No args. The agent's role and what it holds.",
        "equip": "args: handler_module:str (req), id:str?, setup:dict? (record fields, no wiring keys), can_call:dict?, callable_by:dict? (keys: $self, $parent, $subtree, the worker or an agent already under it). Places ONE agent under this cabinet's worker, granted inside the cabinet only. Returns {id, parent, handler_module}.",
        "open_cabinet": "args: task:str (req), name:str?, backend:str? (an existing AI agent id; default the master), setup:dict? (fields over the backend's copy; a new backend is flagged). Opens one cabinet: a subcanvas with its worker AI, wired to every live worker and to the master. The worker is NOT started — send it its first message (the task). Returns {id, worker, master, backend, new_backend, peers}.",
        "close_cabinet": "args: id:str (req) — a cabinet of this manager. Deletes the cabinet with its worker and ALL its equipment, then rewrites the live workers' directories. Returns {closed, deleted}.",
        "cabinets": "No args. Every live cabinet: its task, worker, live status and equipment. The master's monitor.",
        "conclude": "No args. Closes EVERY cabinet this manager opened, deleting each subcanvas with its worker and equipment. The manager stays. Returns {concluded, deleted}.",
        "status": "args: line:str (req), progress:number 0-100?, result:any?. The worker's report up: stored on the cabinet and emitted on its own id, so its view and every watcher see it live.",
        "view": "No args. The cabinet's panel: its task, worker, live status and equipment.",
    })
}

/// The cabinet bundle.
#[derive(Default)]
pub struct CabinetBundle;

#[async_trait]
impl Bundle for CabinetBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
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
                "close_cabinet" => close(id, payload, kernel).await,
                "cabinets" => cabinets_verb(id, kernel),
                "conclude" => conclude(id, kernel).await,
                _ => unknown_verb(&t, MANAGER),
            },
            Some(CABINET) => match verb {
                "reflect" => reflect(id, kernel),
                "status" => status(id, payload, kernel).await,
                "equip" => equip(id, payload, kernel).await,
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
