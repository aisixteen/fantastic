//! THE MEMORY STORE — this bundle owns no rows and no files.
//!
//! A rule-for-rule port of python `ai_core/core.py`'s memory layer (`_mem`,
//! `_store_of`, `_store_call`, `_head`, `_refresh_head`, `_load_history`,
//! `_save_history`, `_store_reply`, `_load_reply`, `_save_view`, `_load_view`,
//! the sharing helpers and the row event), which is canonical.
//!
//! An AI agent's conversation lives in the MEMORY STORE its record names
//! (`memory_store_id`) — a separate agent (`ram_store`, or a durable one) that keys
//! every row by WHO WROTE IT. So this agent reads and writes ITS OWN rows only, by
//! plain gated sends: the creator wires `can_call` to the store and the store's
//! `callable_by` to this agent, explicitly. No store named ⇒ the agent fails loud
//! at boot and at its first turn; a store that refuses or is unreachable mid-turn ⇒
//! the turn fails fast. Nothing falls back to RAM.
//!
//! THE HEAD CACHE. The rows from the last `clear` on (the head) are read ONCE, on
//! the first turn after boot, and then kept in RAM WRITE-THROUGH: every row is
//! appended to the store FIRST and enters the cache only when the store took it, so
//! the cache never holds what the store does not. Reads that reach behind the head
//! (recall, grep, `history {fold}`) go to the store.
//!
//! BODIES ARE NEVER PERSISTED. A reply too large to inline is kept in RAM under its
//! pid for this incarnation only; what the store keeps is the reply's `origin`
//! line (how to get it again). After a restart there are no bodies — the model
//! re-reads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fantastic_bundle::{py_json_dumps, py_repr, py_str, py_truthy};
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

use crate::helpers::agent_meta;

/// The memory store is missing, refused, or unreachable — fail fast. Python
/// `MemoryStoreError(RuntimeError)`.
#[derive(Debug, Clone)]
pub struct MemoryStoreError(pub String);

impl std::fmt::Display for MemoryStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A MemoryStoreError leaving a verb handler is a RAISE: the kernel settles it
/// `failed` with `{error: "MemoryStoreError: …", raised: true}` — python's
/// `f"{type(e).__name__}: {e}"`.
#[derive(Debug)]
pub struct Raised(pub MemoryStoreError);

impl std::fmt::Display for Raised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MemoryStoreError: {}", self.0 .0)
    }
}

impl std::error::Error for Raised {}

impl From<MemoryStoreError> for fantastic_kernel::bundle::BundleError {
    fn from(e: MemoryStoreError) -> Self {
        Box::new(Raised(e))
    }
}

/// Shorthand for the memory layer's results.
pub type MemResult<T> = Result<T, MemoryStoreError>;

/// This bundle's RAM for ONE kernel — the head caches, the bodies held this run,
/// the row watchers — kept ON the kernel's context, so two
/// kernels in one process never share them. Python `_mem(kernel)`.
#[derive(Default)]
pub struct Mem {
    pub(crate) heads: Mutex<HashMap<String, Vec<Value>>>,
    /// `agent → [(pid, body)]`, in the order the bodies arrived (python's dict).
    pub(crate) bodies: Mutex<HashMap<String, Vec<(i64, String)>>>,
    /// `agent → [(watcher id, wants bodies)]`, in registration order.
    pub(crate) watchers: Mutex<HashMap<String, Vec<(String, bool)>>>,
}

/// The memory layer's state on `kernel`.
#[must_use]
pub fn mem(kernel: &Kernel) -> Arc<Mem> {
    kernel.extension::<Mem>("ai_core.memory")
}

/// The store the record names, or the loud refusal. Python `_store_of`.
pub fn store_of(self_id: &AgentId, kernel: &Kernel) -> MemResult<AgentId> {
    let meta = agent_meta(self_id, kernel);
    match meta.get("memory_store_id").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(AgentId::from(s)),
        _ => Err(MemoryStoreError(format!(
            "{self_id}: no `memory_store_id` — an AI agent keeps its conversation in a \
             memory store and runs without none. Create one (e.g. a ram_store), set \
             memory_store_id on {self_id}, and grant {self_id} can_call on it AND the store \
             callable_by for it"
        ))),
    }
}

/// ONE gated send to this agent's memory store, as this agent — it runs in the
/// agent's own code (its dispatch, a task that dispatch spawned, or one of its
/// hooks), so the send is the agent's. A refusal or a failed dispatch is a
/// [`MemoryStoreError`] — the turn fails fast. The store keys a row by its
/// writer: this agent's own memory.
pub async fn store_call(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    payload: Value,
) -> MemResult<Map<String, Value>> {
    let sid = store_of(self_id, kernel)?;
    let verb = payload
        .get("type")
        .map_or_else(|| "None".to_string(), py_str);
    let out = kernel
        .send(vec![Item::new(sid.clone(), payload)])
        .await
        .remove(0);
    let res = out.result;
    let refused = res.get("error").is_some_and(py_truthy);
    if !out.status.is_completed_or(refused) {
        let err = match &res {
            Value::Object(m) => m.get("error").map_or_else(|| "None".to_string(), py_str),
            _ => out.status.as_str().to_string(),
        };
        let hint = res
            .get("hint")
            .filter(|h| py_truthy(h))
            .map(|h| format!(" (hint: {})", py_str(h)))
            .unwrap_or_default();
        return Err(MemoryStoreError(format!(
            "{self_id}: memory store {} {verb} failed: {err}{hint}",
            py_repr(&sid.0)
        )));
    }
    Ok(match res {
        Value::Object(m) => m,
        _ => Map::new(),
    })
}

/// `status == completed and not refused` — the one success test.
trait Completed {
    fn is_completed_or(&self, refused: bool) -> bool;
}

impl Completed for fantastic_kernel::Status {
    fn is_completed_or(&self, refused: bool) -> bool {
        *self == fantastic_kernel::Status::Completed && !refused
    }
}

fn rows_of(res: &Map<String, Value>, self_id: &AgentId) -> Vec<Value> {
    res.get("rows")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|r| {
                    let mut m = r.as_object().cloned().unwrap_or_default();
                    m.insert("agent".to_string(), Value::from(self_id.0.clone()));
                    Value::Object(m)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The store holds this agent's conversation AND its view rows. The CONVERSATION
/// a reader sees is everything but the view (an allow-list of kinds to HIDE: a
/// kind forgotten here lands in the model's prompt).
const BOOKKEEPING: [&str; 1] = ["view"];

/// The chat roles a TURN carries.
const TURN_ROLES: [&str; 3] = ["user", "assistant", "system"];

fn kind_of(r: &Value) -> Option<&str> {
    r.get("kind").and_then(Value::as_str)
}

/// The head cache — loaded from the store on first use (lazy boot). Python
/// `_head`.
pub async fn head(self_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<Vec<Value>> {
    let m = mem(kernel);
    if let Some(h) = m.heads.lock().expect("heads poisoned").get(&self_id.0) {
        return Ok(h.clone());
    }
    let res = store_call(
        self_id,
        kernel,
        json!({"type": "read", "from_last_kind": "clear"}),
    )
    .await?;
    let h = rows_of(&res, self_id);
    m.heads
        .lock()
        .expect("heads poisoned")
        .entry(self_id.0.clone())
        .or_insert_with(|| h.clone());
    Ok(h)
}

/// Is the head cache warm for this agent?
#[must_use]
pub fn head_loaded(self_id: &AgentId, kernel: &Kernel) -> bool {
    mem(kernel)
        .heads
        .lock()
        .expect("heads poisoned")
        .contains_key(&self_id.0)
}

/// THE STORE IS THE TRUTH; the head cache must never diverge from it. The cache
/// holds this agent's own writes (write-through), but another writer may append
/// under this key too — an admin planting a row, a store fed from outside. So each
/// read of the head first reads every row AFTER the newest cached id and merges it
/// by id (a row the agent wrote meanwhile is already there). A `clear` among them
/// restarts the head. One small read per turn; the store stays passive (no push).
pub async fn refresh_head(self_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<Vec<Value>> {
    let m = mem(kernel);
    let cached = m
        .heads
        .lock()
        .expect("heads poisoned")
        .get(&self_id.0)
        .cloned();
    let ids: Vec<i64> = cached
        .as_ref()
        .map(|h| {
            h.iter()
                .filter_map(|r| r.get("id").and_then(Value::as_i64))
                .collect()
        })
        .unwrap_or_default();
    let Some(newest) = ids.iter().copied().max() else {
        m.heads.lock().expect("heads poisoned").remove(&self_id.0);
        return head(self_id, kernel).await;
    };
    let res = store_call(self_id, kernel, json!({"type": "read", "after": newest})).await?;
    let fresh = rows_of(&res, self_id);
    let mut heads = m.heads.lock().expect("heads poisoned");
    let h = heads.entry(self_id.0.clone()).or_default();
    let mut seen: std::collections::HashSet<i64> = ids.into_iter().collect();
    for r in fresh {
        let rid = r.get("id").and_then(Value::as_i64);
        if rid.is_some_and(|i| seen.contains(&i)) {
            continue;
        }
        if kind_of(&r) == Some("clear") {
            h.clear();
        }
        if let Some(i) = rid {
            seen.insert(i);
        }
        h.push(r);
    }
    Ok(h.clone())
}

/// The agent's conversation, in order. AGENT ID IS CONVERSATION ID: a second
/// conversation is a second agent. Python `_load_history`.
///
/// A CLEAR IS A BOUNDARY, NOT A DELETION (`context_clear`): the head starts at the
/// LAST `clear` row. `behind = true` (recall, grep, `history {fold}` — the verbs
/// that reach what is not in view) reads the WHOLE conversation from the store.
pub async fn load_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    behind: bool,
) -> MemResult<Vec<Value>> {
    let rows = if behind {
        let res = store_call(self_id, kernel, json!({"type": "read"})).await?;
        rows_of(&res, self_id)
    } else {
        refresh_head(self_id, kernel).await?
    };
    // THE CONVERSATION IS ITS TURNS — rows with a chat `role`. The agent may also
    // `append` a row of its own straight to its store (a note to itself); that row
    // is memory, not a turn: it stays in the store, `grep` finds it, the view never
    // renders it as a message it cannot be.
    let rows: Vec<Value> = rows
        .into_iter()
        .filter(|r| !kind_of(r).is_some_and(|k| BOOKKEEPING.contains(&k)))
        .filter(|r| {
            r.get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| TURN_ROLES.contains(&role))
        })
        .collect();
    Ok(rows)
}

/// APPEND rows to the memory store, then to the head cache (write-through). A row
/// without an id takes the kernel's next one. A `clear` restarts the head.
/// Returns the ids. Python `_save_history`.
pub async fn save_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    new_turns: &[Value],
) -> MemResult<Vec<i64>> {
    let mut rows: Vec<Map<String, Value>> = Vec::with_capacity(new_turns.len());
    for t in new_turns {
        let mut r: Map<String, Value> = match t {
            Value::Object(m) => m
                .iter()
                .filter(|(k, _)| k.as_str() != "agent")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            other => {
                let mut m = Map::new();
                m.insert("content".to_string(), Value::from(py_str(other)));
                m
            }
        };
        if r.get("id").is_none_or(Value::is_null) {
            r.insert("id".to_string(), Value::from(kernel.mint_id()));
        }
        rows.push(r);
    }
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect();
    store_call(
        self_id,
        kernel,
        json!({"type": "append", "rows": rows.iter().cloned().map(Value::Object).collect::<Vec<_>>()}),
    )
    .await?;
    let m = mem(kernel);
    for r in rows {
        let mut stored = r;
        stored.insert("agent".to_string(), Value::from(self_id.0.clone()));
        let stored = Value::Object(stored);
        {
            let mut heads = m.heads.lock().expect("heads poisoned");
            if let Some(h) = heads.get_mut(&self_id.0) {
                if kind_of(&stored) == Some("clear") {
                    h.clear();
                }
                h.push(stored.clone());
            }
        }
        if !kind_of(&stored).is_some_and(|k| BOOKKEEPING.contains(&k)) {
            announce_row(self_id, kernel, &stored);
        }
    }
    Ok(ids)
}

/// Drop a deleted agent's head, bodies and watchers on this kernel
/// ([`crate::verbs::on_delete`]).
pub fn forget(self_id: &AgentId, kernel: &Kernel) {
    let m = mem(kernel);
    m.heads.lock().expect("heads poisoned").remove(&self_id.0);
    m.bodies.lock().expect("bodies poisoned").remove(&self_id.0);
    m.watchers
        .lock()
        .expect("watchers poisoned")
        .remove(&self_id.0);
}

/// Drop this agent's head cache — a new incarnation reads its
/// store afresh (`boot`). Bodies are this run's only and stay.
pub fn forget_head(self_id: &AgentId, kernel: &Kernel) {
    let m = mem(kernel);
    m.heads.lock().expect("heads poisoned").remove(&self_id.0);
}

/// A reply too big to hand the model as a turn: kept IN RAM under its pid (never
/// persisted); the model is handed its digest + `origin` line. Python
/// `_store_reply`.
pub fn store_reply(self_id: &AgentId, kernel: &Kernel, pid: i64, body: &str) -> bool {
    let m = mem(kernel);
    let mut all = m.bodies.lock().expect("bodies poisoned");
    let list = all.entry(self_id.0.clone()).or_default();
    match list.iter_mut().find(|(p, _)| *p == pid) {
        Some(slot) => slot.1 = body.to_string(),
        None => list.push((pid, body.to_string())),
    }
    true
}

/// Every body held this run for this agent, `(pid, text)` in arrival order.
#[must_use]
pub fn bodies(self_id: &AgentId, kernel: &Kernel) -> Vec<(i64, String)> {
    mem(kernel)
        .bodies
        .lock()
        .expect("bodies poisoned")
        .get(&self_id.0)
        .cloned()
        .unwrap_or_default()
}

/// The agent's STATE: the ordered ids it sees, each at a resolution — a `view`
/// row in its memory store; the newest one in the head is current. Python
/// `_save_view`.
pub async fn save_view(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    ids: &[Value],
    budget: i64,
    why: &str,
) -> MemResult<()> {
    save_history(
        self_id,
        kernel,
        &[json!({
            "kind": "view",
            "v": 1,
            "ts": crate::helpers::now_secs(),
            "budget": budget,
            "why": why,
            "ids": ids,
        })],
    )
    .await
    .map(|_| ())
}

/// The current view doc, when one has been written and carries an `ids` list.
/// Python `_load_view`.
pub async fn load_view(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> MemResult<Option<Map<String, Value>>> {
    for r in head(self_id, kernel).await?.iter().rev() {
        if kind_of(r) == Some("view") {
            return Ok(r
                .as_object()
                .filter(|m| m.get("ids").is_some_and(Value::is_array))
                .cloned());
        }
    }
    Ok(None)
}

// ROW EVENT WATCHERS — the kernel holds no rows, so this agent announces its own:
// `watchers[agent] = [(watcher id, wants bodies)]`, registered by the `watch` verb
// (the agent's own gate decides who may call it); a watcher whose inbox is gone is
// dropped.

/// A content-free digest of one row — `{id, kind, role, bytes}`. Python
/// `_row_digest`.
#[must_use]
pub fn row_digest(row: &Value) -> Value {
    let text = match row.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_json_dumps(other, true, None),
        None => "null".to_string(),
    };
    let kind = match row.get("kind") {
        Some(k) if py_truthy(k) => k.clone(),
        _ => row.get("role").cloned().unwrap_or(Value::Null),
    };
    let mut d = Map::new();
    d.insert(
        "id".to_string(),
        row.get("id").cloned().unwrap_or(Value::Null),
    );
    d.insert("kind".to_string(), kind);
    d.insert(
        "role".to_string(),
        row.get("role").cloned().unwrap_or(Value::Null),
    );
    d.insert("bytes".to_string(), Value::from(text.chars().count()));
    Value::Object(d)
}

/// Register `watcher` for this agent's row events. Python `_watch`'s bookkeeping.
pub fn add_watcher(self_id: &AgentId, kernel: &Kernel, watcher: &str, body: bool) {
    let m = mem(kernel);
    let mut ws = m.watchers.lock().expect("watchers poisoned");
    let list = ws.entry(self_id.0.clone()).or_default();
    match list.iter_mut().find(|(w, _)| w == watcher) {
        Some(slot) => slot.1 = body,
        None => list.push((watcher.to_string(), body)),
    }
}

/// Stop announcing to `watcher`.
pub fn remove_watcher(self_id: &AgentId, kernel: &Kernel, watcher: &str) {
    let m = mem(kernel);
    let mut ws = m.watchers.lock().expect("watchers poisoned");
    if let Some(list) = ws.get_mut(&self_id.0) {
        list.retain(|(w, _)| w != watcher);
    }
}

/// Put the row event in every watcher's inbox: a content-free digest, plus the
/// content for a `body:true` watcher. Python `_announce_row`.
pub fn announce_row(self_id: &AgentId, kernel: &Arc<Kernel>, row: &Value) {
    let m = mem(kernel);
    let list = match m
        .watchers
        .lock()
        .expect("watchers poisoned")
        .get(&self_id.0)
    {
        Some(l) if !l.is_empty() => l.clone(),
        _ => return,
    };
    let mut gone: Vec<String> = Vec::new();
    for (wid, body) in list {
        let Some(q) = kernel
            .inboxes
            .get(&AgentId::from(wid.as_str()))
            .map(|e| e.clone())
        else {
            gone.push(wid);
            continue;
        };
        let mut digest = row_digest(row);
        if body {
            if let Some(d) = digest.as_object_mut() {
                d.insert(
                    "content".to_string(),
                    row.get("content").cloned().unwrap_or(Value::Null),
                );
            }
        }
        let mut ev = Map::new();
        ev.insert("type".to_string(), Value::from("row"));
        ev.insert("source".to_string(), Value::from(self_id.0.clone()));
        ev.insert("row".to_string(), digest);
        q.send(fantastic_kernel::inbox::InboxItem::text(Value::Object(ev)));
    }
    if !gone.is_empty() {
        let mut ws = m.watchers.lock().expect("watchers poisoned");
        if let Some(l) = ws.get_mut(&self_id.0) {
            l.retain(|(w, _)| !gone.contains(w));
        }
    }
}
