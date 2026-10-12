//! THE EXECUTION ROWS — the `call`, `settle`, `task`, `ack` rows the kernel mints
//! and has NOT YET FLUSHED to its memory store, id-addressed by the one counter.
//!
//! A rule-for-rule port of python's `kernel/_kernel.py` (`rows`, `rows_put`,
//! `rows_stream`, `mint_id`), which is canonical.
//!
//! THE KERNEL KEEPS NO CONVERSATION. An AI agent's rows live in that agent's own
//! memory store; this table holds only the kernel's execution bookkeeping, and
//! only until the kernel's flusher (`Kernel::flush_executions`) writes it into the
//! root's memory store — then the rows leave RAM. A kernel that was never started
//! keeps them here until the process ends.
//!
//! THE ONE COUNTER lives here too. Every id in the kernel — a call, a forked task,
//! a settle, a conversation row an AI agent writes to its store — is the next value
//! of this int64. It is PERSISTED BY BLOCK RESERVATION beside `guid` in the root
//! record: `load` reserves a block past the persisted bound, `kernel_start` writes
//! that bound into the root record (`next_id`), and a started kernel minting past it
//! reserves and writes the next block (`Kernel::mint_id`).

use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::sync::Mutex;
use tokio::sync::mpsc;

use crate::agent::AgentId;

/// One execution row. An OPEN map, as python's row is an open dict: `kind`,
/// `verb`, `parent`, `caller`, `payload`, `outcome` and whatever else the kernel
/// writes all ride in the same shape.
pub type Row = Map<String, Value>;

/// How many ids one reservation covers — python `Kernel.ID_BLOCK`.
pub const ID_BLOCK: i64 = 1000;

struct Inner {
    next_id: i64,
    /// The bound the root record holds (or will hold, once started): ids below it
    /// are reserved. Minting AT it reserves the next block.
    id_reserved: i64,
    rows: BTreeMap<i64, Row>,
    tails: Vec<mpsc::UnboundedSender<i64>>,
}

/// The table. ONE mutex: minting an id and inserting the row must be a single
/// atomic step — python gets that from the GIL.
pub struct RowStore {
    inner: Mutex<Inner>,
}

impl Default for RowStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RowStore {
    /// An empty table, counter at 1 — the kernel's starting state, always.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_id: 1,
                id_reserved: 1,
                rows: BTreeMap::new(),
                tails: Vec::new(),
            }),
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

    /// The reserved bound — what `kernel_start` writes into the root record.
    #[must_use]
    pub fn id_reserved(&self) -> i64 {
        self.lock().id_reserved
    }

    /// THE COUNTER COMES BACK WITH THE GUID (`Kernel.load`): the root record's
    /// reserved bound is where this run starts (never lowered), and this run's
    /// first block is reserved in memory. Nothing is written — a record is never
    /// edited by a load.
    pub fn restore_counter(&self, persisted: Option<i64>) {
        let mut inner = self.lock();
        if let Some(n) = persisted.filter(|n| *n > 0) {
            inner.next_id = inner.next_id.max(n);
        }
        inner.id_reserved = inner.next_id + ID_BLOCK;
    }

    /// Mint one id. Returns `(id, new_bound)`: `new_bound` is `Some` when this mint
    /// reached the reserved bound and a new block was reserved — the caller
    /// persists it (only a STARTED kernel does, see `Kernel::mint_id`).
    pub(crate) fn mint(&self) -> (i64, Option<i64>) {
        let mut inner = self.lock();
        mint_locked(&mut inner)
    }

    /// Append one EXECUTION row (`agent` = the agent it is about); mint its id when
    /// absent, accept a PRE-MINTED unused one, refuse anything else — the kernel is
    /// the only allocator. Returns `(id, new_bound)` like [`Self::mint`].
    pub(crate) fn put(&self, agent: &AgentId, row: Row) -> Result<(i64, Option<i64>), String> {
        let mut inner = self.lock();
        let (rid, bound) = match row.get("id") {
            None | Some(Value::Null) => mint_locked(&mut inner),
            Some(v) => {
                let n = v.as_i64().filter(|_| !v.is_boolean());
                match n {
                    Some(n) if n > 0 && n < inner.next_id && !inner.rows.contains_key(&n) => {
                        (n, None)
                    }
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
        // A DROPPED TAIL LEAVES A DEAD SENDER. Python removes the queue in the
        // generator's `finally`; here the receiver's drop closes the channel and
        // the next append prunes it.
        inner.tails.retain(|tx| tx.send(rid).is_ok());
        inner.rows.insert(rid, stored);
        Ok((rid, bound))
    }

    /// One row by id.
    #[must_use]
    pub fn get_one(&self, id: i64) -> Option<Row> {
        self.lock().rows.get(&id).cloned()
    }

    /// Every row still in RAM, oldest id first.
    #[must_use]
    pub fn all(&self) -> Vec<Row> {
        self.lock().rows.values().cloned().collect()
    }

    /// Every id still in RAM, ascending.
    #[must_use]
    pub fn ids(&self) -> Vec<i64> {
        self.lock().rows.keys().copied().collect()
    }

    /// `true` while `id` is held in RAM.
    #[must_use]
    pub fn contains(&self, id: i64) -> bool {
        self.lock().rows.contains_key(&id)
    }

    /// Drop one row (it was flushed). `None` when it was not held.
    pub fn remove(&self, id: i64) -> Option<Row> {
        self.lock().rows.remove(&id)
    }

    /// STAMP a field onto a row that is still in RAM — the call row's `outcome`
    /// at settle, its `attempts` at a retry. A flushed row is not edited: the store
    /// holds what was written.
    pub fn stamp(&self, id: i64, stamp: &Row) {
        let mut inner = self.lock();
        if let Some(row) = inner.rows.get_mut(&id) {
            for (k, v) in stamp {
                row.insert(k.clone(), v.clone());
            }
        }
    }

    /// THE TAIL. Every row still in RAM with `id > after` in id order, then every
    /// row as it is appended. Replay then live, deduped by id. Yielding is not a
    /// message: no send, no task, no row.
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

    /// How many tails are registered (a dropped one is pruned at the next append).
    #[must_use]
    pub fn tail_count(&self) -> usize {
        self.lock().tails.iter().filter(|t| !t.is_closed()).count()
    }
}

fn mint_locked(inner: &mut Inner) -> (i64, Option<i64>) {
    let n = inner.next_id;
    inner.next_id = n + 1;
    if n >= inner.id_reserved {
        inner.id_reserved = n + ID_BLOCK;
        return (n, Some(inner.id_reserved));
    }
    (n, None)
}

/// A live subscription: the replay, then the appends.
pub struct RowTail {
    rx: mpsc::UnboundedReceiver<i64>,
    replay: std::vec::IntoIter<i64>,
    /// The REPLAY set only — an id appended live is yielded once.
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
            if id <= self.after || !self.seen.insert(id) {
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

#[cfg(test)]
#[path = "rows/tests.rs"]
mod tests;
