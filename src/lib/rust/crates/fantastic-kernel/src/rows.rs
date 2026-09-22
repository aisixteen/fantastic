//! THE MESSAGE STORE — every message the kernel ever made, id-addressed by the
//! one counter, append-only: a row is never edited or removed by this API.
//!
//! A rule-for-rule port of python's `kernel/_kernel.py:430-545`, which is
//! canonical.
//!
//! An agent's CONVERSATION (a message, a delivered reply, a fold mark, a recall
//! decision) and the kernel's own bookkeeping — a `call` row per batch item, a
//! `task` row per forked job — live in the same table; readers filter by `kind`.
//! A dehydrated reply BODY is a `body` row whose `parent` is the call's id; an
//! agent's VIEW (the ordered ids it sees, each at a resolution) is a `view` row,
//! the newest one being current. ONE TABLE, so one append-only stream carries
//! everything. Rows outlive `delete_agent`: agent id is conversation id, and a
//! re-created id continues where it left off — a clean slate is a new id.
//!
//! THE KERNEL NEVER KNOWS DISK. It starts empty, always. A hydrator (a loader
//! bundle, from above) fills it with [`RowStore::put`] and sets the counter; a
//! persister tails it with [`RowStore::subscribe`]. Both are optional and
//! interchangeable.

use fantastic_bundle::{py_str, py_truthy};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::agent::AgentId;
use crate::kernel::StateSubs;

/// One stored message. An OPEN map, as python's row is an open dict: `kind`,
/// `role`, `parent`, `content`, `title`, `level`, `refs`, `verb`, `origin`,
/// `status` and whatever else a bundle writes all ride in the same shape, and
/// both readers (the `messages` verb and the log writer) walk it generically.
pub type Row = Map<String, Value>;

#[derive(Default)]
struct Inner {
    next_id: i64,
    rows: BTreeMap<i64, Row>,
    by_agent: HashMap<AgentId, Vec<i64>>,
    body_by_parent: HashMap<i64, i64>,
    view_by_agent: HashMap<AgentId, i64>,
    tails: Vec<mpsc::UnboundedSender<i64>>,
}

/// The store. ONE mutex: minting an id, inserting the row and updating the three
/// indexes must be a single atomic step — python gets that from the GIL, and a
/// sharded map would let a reader see an id in `by_agent` before the row is in
/// `rows`.
pub struct RowStore {
    inner: Mutex<Inner>,
    /// The kernel's state taps, so an append can publish THE ROW EVENT. `None`
    /// for a bare store (the store's own unit tests); every kernel wires it.
    state: Option<Arc<StateSubs>>,
}

impl Default for RowStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RowStore {
    /// An empty store, counter at 1 — the kernel's starting state, always.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_id: 1,
                ..Inner::default()
            }),
            state: None,
        }
    }

    /// The kernel's store: an empty store that PUBLISHES each append as a `row`
    /// state event on the kernel's own registry.
    #[must_use]
    pub(crate) fn with_state(state: Arc<StateSubs>) -> Self {
        Self {
            state: Some(state),
            ..Self::new()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("row store poisoned")
    }

    /// The id the next mint will hand out.
    #[must_use]
    pub fn next_id(&self) -> i64 {
        self.lock().next_id
    }

    /// HYDRATOR ONLY: raise the counter before the first mint, so every hydrated
    /// id is a pre-minted one. Never lowers it.
    pub fn set_next_id(&self, n: i64) {
        let mut inner = self.lock();
        inner.next_id = inner.next_id.max(n);
    }

    /// The next message id. The ONLY allocator in the kernel: a call, a forked
    /// task, a conversation row, a reply body all take their id here, so one
    /// number names one thing everywhere (`ps`, `cancel`, `messages`, a reply
    /// tag). Minting RESERVES the id; the row may be written later ([`Self::put`]
    /// accepts a pre-minted id) — an AI message names its own id in the reply tags
    /// it sends before its row exists.
    pub fn mint_id(&self) -> i64 {
        let mut inner = self.lock();
        let n = inner.next_id;
        inner.next_id = n + 1;
        n
    }

    /// Append one message for `agent`; mint its id when absent, accept a
    /// PRE-MINTED unused one, refuse anything else — the store is the only
    /// allocator. Bookkeeping from above (the same standing as the ledger),
    /// never gated. Returns the id.
    pub fn put(&self, agent: &AgentId, row: Row) -> Result<i64, String> {
        let mut inner = self.lock();
        let rid = match row.get("id") {
            None | Some(Value::Null) => {
                let n = inner.next_id;
                inner.next_id = n + 1;
                n
            }
            Some(v) => {
                let n = v.as_i64().filter(|_| !v.is_boolean());
                match n {
                    Some(n) if n > 0 && n < inner.next_id && !inner.rows.contains_key(&n) => n,
                    _ => {
                        return Err(format!(
                            "rows_put: id must be absent or a pre-minted unused id, got {v}"
                        ))
                    }
                }
            }
        };
        let mut stored = row;
        stored.insert("id".to_string(), Value::from(rid));
        stored.insert("agent".to_string(), Value::from(agent.0.clone()));
        stored
            .entry("ts".to_string())
            .or_insert_with(|| Value::from(now_secs()));
        match stored.get("kind").and_then(Value::as_str) {
            Some("body") => {
                if let Some(p) = stored.get("parent").and_then(Value::as_i64) {
                    inner.body_by_parent.insert(p, rid);
                }
            }
            Some("view") => {
                inner.view_by_agent.insert(agent.clone(), rid);
            }
            _ => {}
        }
        inner.by_agent.entry(agent.clone()).or_default().push(rid);
        // A DROPPED TAIL LEAVES A DEAD SENDER. Python removes the queue in the
        // generator's `finally`; here the receiver's drop closes the channel and
        // the next append prunes it.
        inner.tails.retain(|tx| tx.send(rid).is_ok());
        // THE ROW EVENT — a view learns "row <id> arrived" reactively on the
        // state stream it already holds, and pulls the body when it needs it.
        // DIGEST ONLY: the content stays behind `pull` / `rows_read`, so a
        // stream never bypasses the gate. Published AFTER storing and after
        // feeding the tails, and OUTSIDE the store lock — a subscriber runs
        // arbitrary code on this thread and may read the store back.
        let digest = self.state.is_some().then(|| row_digest(rid, &stored));
        inner.rows.insert(rid, stored);
        drop(inner);
        if let (Some(subs), Some(digest)) = (self.state.as_ref(), digest) {
            let mut event = Map::new();
            event.insert("agent_id".to_string(), Value::from(agent.0.clone()));
            event.insert("kind".to_string(), Value::from("row"));
            event.insert("row".to_string(), digest);
            crate::kernel::publish_state_to(subs, &Value::Object(event), None);
        }
        Ok(rid)
    }

    /// That agent's rows, in append order.
    #[must_use]
    pub fn for_agent(&self, agent: &AgentId) -> Vec<Row> {
        let inner = self.lock();
        inner
            .by_agent
            .get(agent)
            .map(|ids| {
                ids.iter()
                    .filter_map(|i| inner.rows.get(i).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Rows by id, any agent, IN THE ORDER ASKED. Missing ids are skipped; the
    /// `messages` verb gates.
    #[must_use]
    pub fn get(&self, ids: &[i64]) -> Vec<Row> {
        let inner = self.lock();
        ids.iter()
            .filter_map(|i| inner.rows.get(i).cloned())
            .collect()
    }

    /// One row by id.
    #[must_use]
    pub fn get_one(&self, id: i64) -> Option<Row> {
        self.lock().rows.get(&id).cloned()
    }

    /// STAMP AN OUTCOME onto a `call` row that is already stored — the one
    /// mutation this API allows, and it does NOT re-notify the tails. Python
    /// mutates the queued dict in place, so a persister reading at flush time
    /// sees the stamp; a second notification would write the row twice.
    pub fn stamp(&self, id: i64, stamp: &Row) {
        let mut inner = self.lock();
        if let Some(row) = inner.rows.get_mut(&id) {
            for (k, v) in stamp {
                row.insert(k.clone(), v.clone());
            }
        }
    }

    /// A reply body too large to inline: a `body` row under the conversation
    /// that received it, `parent` = the call's id. Returns the row id.
    pub fn body_put(&self, agent: &AgentId, pid: i64, text: &str) -> Result<i64, String> {
        let mut row = Map::new();
        row.insert("kind".to_string(), Value::from("body"));
        row.insert("parent".to_string(), Value::from(pid));
        row.insert("content".to_string(), Value::from(text));
        self.put(agent, row)
    }

    /// One page of a stored body: `(text, next_offset, eof, size)`.
    #[must_use]
    pub fn body_get(
        &self,
        pid: i64,
        offset: usize,
        length: usize,
    ) -> Option<(String, usize, bool, usize)> {
        let inner = self.lock();
        let rid = *inner.body_by_parent.get(&pid)?;
        let row = inner.rows.get(&rid)?;
        // python: `b = str(row.get("content", ""))` — a non-string body is
        // stringified, not treated as empty.
        let empty = Value::String(String::new());
        let body = fantastic_bundle::py_str(row.get("content").unwrap_or(&empty));
        // CODE POINTS, not bytes: python slices a str, and a byte slice would cut
        // a codepoint in half on any non-ascii body.
        let chars: Vec<char> = body.chars().collect();
        let len = chars.len();
        // python slicing never clamps the offset: `b[offset:offset+max(1,length)]`
        // past the end is simply empty, and `next = offset + len(chunk)` carries
        // the RAW offset forward — it is not pulled back into range.
        let start = offset.min(len);
        let end = offset.saturating_add(length.max(1)).min(len);
        let chunk: String = if start < end {
            chars[start..end].iter().collect()
        } else {
            String::new()
        };
        let next = offset + chunk.chars().count();
        Some((chunk, next, next >= len, len))
    }

    /// The agent's VIEW as a `view` row; the newest one is current.
    pub fn view_put(&self, agent: &AgentId, doc: Row) -> Result<i64, String> {
        let mut row = doc;
        row.insert("kind".to_string(), Value::from("view"));
        self.put(agent, row)
    }

    /// The current view, without the row's own bookkeeping.
    #[must_use]
    pub fn view_get(&self, agent: &AgentId) -> Option<Row> {
        let inner = self.lock();
        let rid = *inner.view_by_agent.get(agent)?;
        let row = inner.rows.get(&rid)?;
        Some(
            row.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "kind" | "agent" | "id" | "parent"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }

    /// THE TAIL. Every row with `id > after` in id order, then every row as it is
    /// appended — for a persister or any other consumer. Replay then live,
    /// deduped by id, so a subscriber that opens at the id it last saw misses
    /// nothing and doubles nothing.
    ///
    /// Registering and snapshotting happen under ONE lock acquisition: python
    /// does them in two statements and is safe under the GIL, which here would be
    /// a window where a row lands in neither half.
    #[must_use]
    pub fn subscribe(&self, after: i64) -> RowTail {
        let mut inner = self.lock();
        let (tx, rx) = mpsc::unbounded_channel();
        inner.tails.push(tx);
        let replay: Vec<i64> = inner
            .rows
            .range((Bound::Excluded(after), Bound::Unbounded))
            .map(|(id, _)| *id)
            .collect();
        RowTail {
            rx,
            seen: replay.iter().copied().collect(),
            replay: replay.into_iter(),
            after,
        }
    }
}

/// A live subscription: the replay, then the appends.
pub struct RowTail {
    rx: mpsc::UnboundedReceiver<i64>,
    replay: std::vec::IntoIter<i64>,
    /// The REPLAY set only.
    ///
    /// IDS DO NOT ARRIVE IN ORDER. An AI message's id is minted when the message starts
    /// and its row lands when the message ends, AFTER the calls it made — so a
    /// high-water filter ("newer than the last id yielded") drops exactly those
    /// messages. Measured in python 2026-09-08: every conversation message missing from
    /// the log. Dedupe against what was replayed, and nothing else.
    seen: HashSet<i64>,
    after: i64,
}

impl RowTail {
    /// The next row id, replay first. `None` only when the store is gone.
    pub async fn recv(&mut self) -> Option<i64> {
        if let Some(id) = self.replay.next() {
            return Some(id);
        }
        loop {
            let id = self.rx.recv().await?;
            if id <= self.after || self.seen.contains(&id) {
                continue;
            }
            return Some(id);
        }
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// THE DIGEST of one row — what the `messages` verb answers and what the `row`
/// state event carries: id, agent, parent, kind, role, a 120-character head and
/// the length, plus the per-kind extras. NEVER the content: bodies stay behind
/// `pull` / `rows_read`, so the gate is not bypassed by a stream.
///
/// A rule-for-rule port of python's `Kernel.row_digest`. Three things about it
/// are easy to get wrong and all three were:
///
/// * `content` is `str(row.get("content", ""))` — an ABSENT key is `""`, every
///   other type is its python `str()` (`None` → `"None"`, `['a', 1, True]`), and
///   `head`/`bytes` count CODE POINTS of that string, not UTF-8 bytes.
/// * every optional key follows PYTHON TRUTHINESS (`if row.get(k):`), and the
///   value copied in is the row's RAW value, whatever its type.
/// * `status` comes from the nested `outcome`, never from a top-level `status`.
///
/// KEY ORDER IS PYTHON'S INSERTION ORDER — `serde_json::Map` is `preserve_order`
/// here, and a view diffs these bytes.
#[must_use]
pub fn row_digest(rid: i64, row: &Row) -> Value {
    let content = row.get("content").map_or_else(String::new, py_str);
    let mut d = Map::new();
    d.insert("id".to_string(), json!(rid));
    d.insert(
        "agent".to_string(),
        row.get("agent").cloned().unwrap_or(Value::Null),
    );
    d.insert(
        "parent".to_string(),
        row.get("parent").cloned().unwrap_or(Value::Null),
    );
    let kind = row.get("kind").cloned().unwrap_or(Value::Null);
    let role = row.get("role").cloned().unwrap_or(Value::Null);
    d.insert(
        "kind".to_string(),
        if py_truthy(&kind) { kind } else { role.clone() },
    );
    d.insert("role".to_string(), role);
    d.insert(
        "head".to_string(),
        Value::from(
            content
                .chars()
                .take(120)
                .collect::<String>()
                .replace('\n', " "),
        ),
    );
    d.insert("bytes".to_string(), json!(content.chars().count()));
    if let Some(t) = row.get("title").filter(|v| py_truthy(v)) {
        d.insert("title".to_string(), t.clone());
    }
    if let Some(l) = row.get("level").filter(|v| py_truthy(v)) {
        d.insert("level".to_string(), l.clone());
    }
    if let Some(r) = row.get("refs").filter(|v| py_truthy(v)) {
        d.insert("refs".to_string(), json!(sized_len(r)));
    }
    match row.get("kind").and_then(Value::as_str) {
        Some("call" | "task") => {
            d.insert(
                "verb".to_string(),
                row.get("verb").cloned().unwrap_or(Value::Null),
            );
            if let Some(n) = row.get("name").filter(|v| py_truthy(v)) {
                d.insert("name".to_string(), n.clone());
            }
            if let Some(outcome) = row.get("outcome").filter(|v| py_truthy(v)) {
                // A non-dict stamp is no status (python guards `.get` since
                // 2026-09-20; it used to raise off the wire).
                d.insert(
                    "status".to_string(),
                    outcome
                        .as_object()
                        .and_then(|o| o.get("status").cloned())
                        .unwrap_or(Value::Null),
                );
            }
        }
        Some("view") => {
            // A VIEW DIGEST SAYS HOW MUCH IT HOLDS, never the array itself.
            let ids = row.get("ids").filter(|v| py_truthy(v));
            d.insert(
                "ids".to_string(),
                json!(match ids {
                    Some(v) => sized_len(v),
                    None => 0,
                }),
            );
            d.insert(
                "budget".to_string(),
                row.get("budget").cloned().unwrap_or(Value::Null),
            );
            // ASSIGNED LAST, and it overwrites a `title` the row already
            // carried — python's plain `d["title"] = ...`, which keeps the
            // earlier key's POSITION when there was one.
            d.insert(
                "title".to_string(),
                row.get("why").cloned().unwrap_or(Value::Null),
            );
        }
        _ => {}
    }
    Value::Object(d)
}

/// `len(x)` for a sized value, `1` for a scalar — a digest counts, it never
/// raises on a row a caller wrote by hand (python, 2026-09-20).
fn sized_len(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        Value::String(s) => s.chars().count(),
        _ => 1,
    }
}

#[cfg(test)]
#[path = "rows/tests.rs"]
mod tests;
