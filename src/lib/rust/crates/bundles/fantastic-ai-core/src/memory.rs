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
//! plain gated sends: the creator writes this agent's line on the store in its own
//! `can`, explicitly. No store named ⇒ the agent fails loud
//! at boot and at its first pass; a store that refuses or is unreachable mid-pass ⇒
//! the pass fails fast. Nothing falls back to RAM.
//!
//! THE HEAD CACHE. The rows from the last `clear` on (the head) are read ONCE, on
//! the first pass after boot, and then kept in RAM WRITE-THROUGH: every row is
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

use fantastic_bundle::{py_json_dumps, py_repr};
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

/// A store's reply and the reply body beside it (the byte verbs' bytes).
pub(crate) type StoreReply = (Map<String, Value>, Option<Vec<u8>>);

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
             memory_store_id on {self_id}, and give {self_id} a line on it \
             (can: {{<store>: {{call: [append, read]}}}})"
        ))),
    }
}

/// TESTS: ONE gated call to this agent's memory store, as this agent, driven to its
/// settle ([`fantastic_kernel::test_support::settled`]) — it runs in the agent's own
/// scope, so the call is the agent's. A refusal or a failed dispatch is a
/// [`MemoryStoreError`]. A pass reads and writes its store as steps
/// ([`crate::steps`]).
#[cfg(any(test, feature = "test-support"))]
pub async fn store_call(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    payload: Value,
) -> MemResult<Map<String, Value>> {
    Ok(store_item(self_id, kernel, payload, None).await?.0)
}

/// TESTS: [`store_call`] with a request body (the raw bytes at `data`), and the reply
/// body beside the reply: the store's byte verbs (`put_bytes`, `get_bytes`).
#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn store_item(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    payload: Value,
    body: Option<Vec<u8>>,
) -> MemResult<StoreReply> {
    let (item, sid, verb) = store_request(self_id, kernel, payload, body)?;
    let out = fantastic_kernel::test_support::settled(kernel, vec![item]).await.remove(0);
    checked(self_id, &sid, &verb, out)
}

/// The call to this agent's memory store: the item, the store and the verb it names.
/// No store named is the `Err`.
pub(crate) fn store_request(
    self_id: &AgentId,
    kernel: &Kernel,
    payload: Value,
    body: Option<Vec<u8>>,
) -> MemResult<(Item, AgentId, String)> {
    let sid = store_of(self_id, kernel)?;
    let verb = payload
        .get("type")
        .map_or_else(|| "null".to_string(), crate::helpers::shown);
    let mut item = Item::new(sid.clone(), payload);
    if let Some(b) = body {
        item = item.with_binary_key("data", b);
    }
    Ok((item, sid, verb))
}

/// A store reply CHECKED, the one rule for every store call: a refusal or a failed
/// dispatch is a [`MemoryStoreError`] naming the store and the verb; else the reply and
/// its body.
pub(crate) fn checked(
    self_id: &AgentId,
    sid: &AgentId,
    verb: &str,
    out: fantastic_kernel::Outcome,
) -> MemResult<StoreReply> {
    let reply_body = out.body.and_then(|b| b.into_at("data"));
    let res = out.result;
    let refused = res.get("error").is_some_and(|e| !e.is_null());
    if !out.status.is_completed_or(refused) {
        let err = match &res {
            Value::Object(m) => m.get("error").map_or_else(|| "null".to_string(), crate::helpers::shown),
            _ => out.status.as_str().to_string(),
        };
        let hint = res
            .get("hint")
            .filter(|h| !h.is_null())
            .map(|h| format!(" (hint: {})", crate::helpers::shown(h)))
            .unwrap_or_default();
        return Err(MemoryStoreError(format!(
            "{self_id}: memory store {} {verb} failed: {err}{hint}",
            py_repr(&sid.0)
        )));
    }
    Ok((
        match res {
            Value::Object(m) => m,
            _ => Map::new(),
        },
        reply_body,
    ))
}

/// TESTS: keep an image's `bytes` in this agent's memory store under `image_ref` (a
/// send's images are kept by its attach steps, [`crate::images::attach`]).
#[cfg(test)]
pub(crate) async fn put_bytes(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    image_ref: &str,
    media_type: &str,
    bytes: Vec<u8>,
) -> MemResult<()> {
    let head = json!({"type": "put_bytes", "ref": image_ref, "media_type": media_type});
    store_item(self_id, kernel, head, Some(bytes)).await?;
    Ok(())
}

/// The bytes this agent's memory store holds under `image_ref`: `(media type, bytes)`.
#[cfg(test)]
pub(crate) async fn get_bytes(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    image_ref: &str,
) -> MemResult<(String, Vec<u8>)> {
    let (head, body) = store_item(
        self_id,
        kernel,
        json!({"type": "get_bytes", "ref": image_ref}),
        None,
    )
    .await?;
    let media = head
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    body.map(|b| (media, b)).ok_or_else(|| {
        MemoryStoreError(format!(
            "{self_id}: memory store returned no bytes for image `{image_ref}`"
        ))
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

pub(crate) fn rows_of(res: &Map<String, Value>, self_id: &AgentId) -> Vec<Value> {
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
/// a reader sees is everything but the view and keep rows (an
/// allow-list of kinds to HIDE: a kind forgotten here lands in the model's prompt).
pub const BOOKKEEPING: [&str; 2] = ["view", "keep"];

/// The chat roles a MESSAGE carries.
pub const CHAT_ROLES: [&str; 3] = ["user", "assistant", "system"];

fn kind_of(r: &Value) -> Option<&str> {
    r.get("kind").and_then(Value::as_str)
}

/// A trace row (#766): the chat's own record of its passes, never in the head.
fn is_trace(r: &Value) -> bool {
    kind_of(r) == Some(crate::model_format::trace::KIND)
}

/// TESTS: the head cache — loaded from the store on first use (lazy boot). Python
/// `_head`. A pass reads it as a step ([`crate::steps`]).
#[cfg(any(test, feature = "test-support"))]
pub async fn head(self_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<Vec<Value>> {
    if let Some(h) = cached_head(self_id, kernel) {
        return Ok(h);
    }
    let read = HeadRead::Whole;
    let res = store_call(self_id, kernel, read.payload()).await?;
    Ok(read.merge(self_id, kernel, &res))
}

/// The head cache as it is, when it is loaded (no store read).
pub(crate) fn cached_head(self_id: &AgentId, kernel: &Kernel) -> Option<Vec<Value>> {
    mem(kernel).heads.lock().expect("heads poisoned").get(&self_id.0).cloned()
}

/// The read that brings the head cache up to the store: the WHOLE head (from the last
/// `clear`) when none is cached, else the rows AFTER its newest id
/// ([`refresh_head`]).
pub(crate) enum HeadRead {
    /// No head cached: it is read from the last `clear` on.
    Whole,
    /// The rows after `newest`, merged by id into the cached head (`ids`).
    After { newest: i64, ids: Vec<i64> },
}

/// What [`refresh_head`] reads now for this agent.
pub(crate) fn head_read(self_id: &AgentId, kernel: &Kernel) -> HeadRead {
    let m = mem(kernel);
    let mut heads = m.heads.lock().expect("heads poisoned");
    let ids: Vec<i64> = heads
        .get(&self_id.0)
        .map(|h| h.iter().filter_map(|r| r.get("id").and_then(Value::as_i64)).collect())
        .unwrap_or_default();
    match ids.iter().copied().max() {
        Some(newest) => HeadRead::After { newest, ids },
        None => {
            heads.remove(&self_id.0);
            HeadRead::Whole
        }
    }
}

impl HeadRead {
    /// The store call that reads it.
    pub(crate) fn payload(&self) -> Value {
        match self {
            Self::Whole => json!({"type": "read", "from_last_kind": "clear"}),
            Self::After { newest, .. } => json!({"type": "read", "after": newest}),
        }
    }

    /// The store's answer `res` taken into the head cache: the head as it is now.
    pub(crate) fn merge(&self, self_id: &AgentId, kernel: &Kernel, res: &Map<String, Value>) -> Vec<Value> {
        let m = mem(kernel);
        match self {
            Self::Whole => {
                // TRACE ROWS ARE NO PART OF THE HEAD (#766): the head does not grow by the cap
                let h: Vec<Value> = rows_of(res, self_id).into_iter().filter(|r| !is_trace(r)).collect();
                m.heads
                    .lock()
                    .expect("heads poisoned")
                    .entry(self_id.0.clone())
                    .or_insert_with(|| h.clone());
                h
            }
            Self::After { ids, .. } => {
                let fresh = rows_of(res, self_id);
                let mut heads = m.heads.lock().expect("heads poisoned");
                let h = heads.entry(self_id.0.clone()).or_default();
                // THE HEAD AS IT IS NOW, not as it was before the read: a row appended while
                // the read was out (a message arriving during a pass, #795) is in both, and
                // is kept once
                let mut seen: std::collections::HashSet<i64> = ids.iter().copied().collect();
                seen.extend(h.iter().filter_map(|r| r.get("id").and_then(Value::as_i64)));
                for r in fresh {
                    let rid = r.get("id").and_then(Value::as_i64);
                    if rid.is_some_and(|i| seen.contains(&i)) || is_trace(&r) {
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
                h.clone()
            }
        }
    }
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
/// restarts the head. One small read per pass; the store stays passive (no push).
/// TESTS: a pass reads it as a step ([`crate::steps`]).
#[cfg(any(test, feature = "test-support"))]
pub async fn refresh_head(self_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<Vec<Value>> {
    let read = head_read(self_id, kernel);
    let res = store_call(self_id, kernel, read.payload()).await?;
    Ok(read.merge(self_id, kernel, &res))
}

/// The agent's conversation, in order. AGENT ID IS CONVERSATION ID: a second
/// conversation is a second agent. Python `_load_history`.
///
/// A CLEAR IS A BOUNDARY, NOT A DELETION (`context_clear`): the head starts at the
/// LAST `clear` row. `behind = true` (recall, grep, `history {fold}` — the verbs
/// that reach what is not in view) reads the WHOLE conversation from the store.
/// TESTS: a pass reads it as a step ([`crate::steps`]).
#[cfg(any(test, feature = "test-support"))]
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
    Ok(chat_rows(rows))
}

/// The conversation among stored `rows`: its messages, in order, each reply block carrying
/// what the model kept of it ([`load_history`]).
pub(crate) fn chat_rows(rows: Vec<Value>) -> Vec<Value> {
    // THE CONVERSATION IS ITS MESSAGES — rows with a chat `role`. The agent may also
    // `append` a row of its own straight to its store (a note to itself); that row
    // is memory, not a message: it stays in the store, `grep` finds it, the view never
    // renders it as a message it cannot be.
    // WHAT THE MODEL KEPT (#641): each `keep` row's summary rides on the reply block it
    // names, in `attributes.keep` (the newest wins); never rendered but by the fold.
    let mut kept: std::collections::HashMap<i64, Value> = std::collections::HashMap::new();
    for r in rows.iter().filter(|r| kind_of(r) == Some("keep")) {
        if let (Some(of), Some(s)) = (r.get("of").and_then(Value::as_i64), r.get("summary")) {
            kept.insert(of, s.clone());
        }
    }
    let mut rows: Vec<Value> = rows
        .into_iter()
        .filter(|r| !kind_of(r).is_some_and(|k| BOOKKEEPING.contains(&k)))
        .filter(|r| {
            r.get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| CHAT_ROLES.contains(&role))
        })
        .collect();
    crate::history_view::attach_kept(&mut rows, &kept);
    rows
}

/// The model's `keep` (#641) checked: the reply block `of` (its pid) is in the conversation
/// the pass read (the head cache its history step loaded). `Err` names why not.
pub(crate) fn keep_check(self_id: &AgentId, kernel: &Kernel, of: i64) -> Result<(), String> {
    let rows = chat_rows(cached_head(self_id, kernel).unwrap_or_default());
    let is_reply = rows.iter().any(|r| {
        r.get("role").and_then(Value::as_str) == Some("user")
            && crate::history_view::reply_blocks(r.get("content").and_then(Value::as_str).unwrap_or(""))
                .into_iter()
                .any(|b| crate::history_view::reply_pid(b) == Some(of))
    });
    if is_reply {
        Ok(())
    } else {
        Err(format!("keep: there is no reply with id {of} in your conversation"))
    }
}

/// APPEND rows to the memory store, then to the head cache (write-through). A row
/// without an id takes the kernel's next one. A `clear` restarts the head.
/// Returns the ids. Python `_save_history`. TESTS: a pass writes as a step
/// ([`crate::steps`]).
#[cfg(any(test, feature = "test-support"))]
pub async fn save_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    new_rows: &[Value],
) -> MemResult<Vec<i64>> {
    let rows = rows_to_append(self_id, kernel, new_rows)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    store_call(self_id, kernel, append_payload(&rows)).await?;
    Ok(appended(self_id, kernel, rows))
}

/// The rows of an append as the store takes them: an id minted where none is given,
/// and WHEN it was written and BY WHOM in each row's `attributes`. A row over the cap is
/// the `Err`.
pub(crate) fn rows_to_append(
    self_id: &AgentId,
    kernel: &Kernel,
    new_rows: &[Value],
) -> MemResult<Vec<Map<String, Value>>> {
    let mut rows: Vec<Map<String, Value>> = Vec::with_capacity(new_rows.len());
    for t in new_rows {
        let mut r: Map<String, Value> = match t {
            Value::Object(m) => m
                .iter()
                .filter(|(k, _)| k.as_str() != "agent")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            other => {
                let mut m = Map::new();
                m.insert("content".to_string(), Value::from(crate::helpers::shown(other)));
                m
            }
        };
        if r.get("id").is_none_or(Value::is_null) {
            r.insert("id".to_string(), Value::from(kernel.mint_id()));
        }
        // WHEN it was written and BY WHOM (the agent whose chat it is, unless the
        // writer named the sender), on every row, in the row's `attributes` (merged
        // into what the writer already put there)
        let attrs = r
            .entry("attributes")
            .and_modify(|a| {
                if !a.is_object() {
                    *a = Value::Object(Map::new());
                }
            })
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(a) = attrs.as_object_mut() {
            let ts = a
                .entry("ts")
                .or_insert_with(|| Value::from(crate::helpers::now_secs()))
                .clone();
            // ITS UTC OFFSET (#766, 1.13), set once, here on append: a row that arrives
            // with one keeps it; none is stored when the C library cannot place `ts`
            if !a.contains_key(crate::row_attrs::UTC_OFFSET) {
                if let Some(off) = crate::row_attrs::offset_at(&ts) {
                    a.insert(crate::row_attrs::UTC_OFFSET.to_string(), off);
                }
            }
            a.entry("source")
                .or_insert_with(|| Value::from(self_id.as_str()));
        }
        crate::row_attrs::check_cap(self_id, &r).map_err(MemoryStoreError)?;
        rows.push(r);
    }
    Ok(rows)
}

/// The store's `append` of `rows`.
pub(crate) fn append_payload(rows: &[Map<String, Value>]) -> Value {
    json!({"type": "append", "rows": rows.iter().cloned().map(Value::Object).collect::<Vec<_>>()})
}

/// The store TOOK `rows`: each enters the head cache (once) and is announced to the
/// agent's watchers. Their ids.
pub(crate) fn appended(self_id: &AgentId, kernel: &Arc<Kernel>, rows: Vec<Map<String, Value>>) -> Vec<i64> {
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect();
    let m = mem(kernel);
    for r in rows {
        let mut stored = r;
        stored.insert("agent".to_string(), Value::from(self_id.0.clone()));
        let stored = Value::Object(stored);
        {
            let mut heads = m.heads.lock().expect("heads poisoned");
            if let Some(h) = heads.get_mut(&self_id.0) {
                // a head refresh that read the store while this append was out took
                // the row already (#795): it is kept once
                let id = stored.get("id").and_then(Value::as_i64);
                let have = id.is_some() && h.iter().any(|x| x.get("id").and_then(Value::as_i64) == id);
                if !have {
                    if kind_of(&stored) == Some("clear") {
                        h.clear();
                    }
                    h.push(stored.clone());
                }
            }
        }
        if !kind_of(&stored).is_some_and(|k| BOOKKEEPING.contains(&k)) {
            announce_row(self_id, kernel, &stored);
        }
    }
    ids
}

/// Purge a deleted agent's rows from its memory store ([`crate::verbs::on_delete`]),
/// unless the delete asked to `keep_memory`. ONE gated `prune` of every row the
/// agent owns (no `keep`: fold, view and bookkeeping rows go too; fold refs and
/// the text index follow in the store). The hook runs AS the agent, so the
/// store's key is the deleted agent's. Delete is not refusable: a store that is
/// missing, already gone or refuses is logged and the cascade goes on.
pub async fn purge(self_id: &AgentId, kernel: &Arc<Kernel>) {
    if fantastic_kernel::lifecycle::keep_memory(kernel, self_id) {
        return;
    }
    let Ok((item, sid, verb)) = store_request(
        self_id,
        kernel,
        json!({"type": "prune", "before_id": i64::MAX}),
        None,
    ) else {
        return;
    };
    // the prune is a call (#795): its settle is read when it comes, and nothing
    // waits for it — the cascade goes on
    let me = self_id.clone();
    kernel
        .call_then(item, move |out| async move {
            if let Err(e) = checked(&me, &sid, &verb, out) {
                println!("  [cascade] {me} memory purge failed: {e}");
            }
        })
        .await;
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

/// A reply too big to hand the model as a message: kept IN RAM under its pid (never
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
///
/// `folds`: the fold rows this view shows that are not stored yet, each under its
/// provisional id ([`crate::folds::provisional_id`]). They and the view row are ONE
/// store append, folds first: a crash leaves both or neither (#766 RS5). Their ids are
/// minted HERE, in fold order, right before that append, and every provisional id is
/// rewritten to its minted one in the folds (`id`, `parent`, `refs`, the head of
/// `content`) and in the view's `ids` ([`crate::folds::remap_ids`]): a row appended
/// while a summarizer ran has a lower id than the folds, and the store's id order after
/// a restart is the order before the stop (#766, fold id order). Returns the map
/// (provisional -> minted), for the caller's own copies of the rows. TESTS: a pass
/// writes its view as a step ([`crate::steps::save_view`]).
#[cfg(any(test, feature = "test-support"))]
pub async fn save_view(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    folds: &[Value],
    ids: &[Value],
    budget: i64,
    why: &str,
) -> MemResult<std::collections::BTreeMap<i64, i64>> {
    let (rows, map) = view_rows(kernel, folds, ids, budget, why);
    save_history(self_id, kernel, &rows).await?;
    Ok(map)
}

/// The rows of a [`save_view`] append — the folds, then the view row — with their ids
/// minted, and the map (provisional -> minted).
pub(crate) fn view_rows(
    kernel: &Kernel,
    folds: &[Value],
    ids: &[Value],
    budget: i64,
    why: &str,
) -> (Vec<Value>, std::collections::BTreeMap<i64, i64>) {
    let map: std::collections::BTreeMap<i64, i64> = folds
        .iter()
        .filter_map(crate::folds::row_id)
        .filter(|id| *id < 0)
        .map(|id| (id, kernel.mint_id()))
        .collect();
    let mut rows = folds.to_vec();
    rows.push(json!({
        "kind": "view",
        "ts": crate::helpers::now_secs(),
        "budget": budget,
        "why": why,
        "ids": ids,
    }));
    for r in &mut rows {
        crate::folds::remap_ids(r, &map);
    }
    (rows, map)
}

/// TESTS: the current view doc, when one has been written and carries an `ids` list.
/// Python `_load_view`.
#[cfg(any(test, feature = "test-support"))]
pub async fn load_view(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
) -> MemResult<Option<Map<String, Value>>> {
    Ok(view_in(&head(self_id, kernel).await?))
}

/// The current view doc among the head's `rows` ([`load_view`]): the newest `view` row,
/// when it carries an `ids` list.
pub(crate) fn view_in(rows: &[Value]) -> Option<Map<String, Value>> {
    rows.iter()
        .rev()
        .find(|r| kind_of(r) == Some("view"))?
        .as_object()
        .filter(|m| m.get("ids").is_some_and(Value::is_array))
        .cloned()
}

/// The current view doc in the head cache, read by a pass after its history step
/// loaded the head ([`view_in`]); `None` when no head is cached.
pub(crate) fn cached_view(self_id: &AgentId, kernel: &Kernel) -> Option<Map<String, Value>> {
    view_in(&cached_head(self_id, kernel)?)
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
        Some(k) if !k.is_null() => k.clone(),
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
