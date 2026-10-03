//! EACH USER'S EVENTS — what its tabs PULL (`events {after}`). One queue per
//! user, shared by all its tabs (each tab keeps its own cursor), bounded: a tab
//! that falls behind the oldest kept event is told `lost` and reloads.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fantastic_kernel::{AgentId, Blob, TaskId};
use serde_json::Value;
use tokio::sync::Notify;

/// Events kept per user.
pub const KEEP: usize = 2000;

/// One event: its sequence number, its JSON, and a raw body when it has one.
#[derive(Clone)]
pub struct Ev {
    pub seq: u64,
    pub v: Value,
    pub body: Option<Blob>,
}

/// A relayed watch: the reader the kernel mirrors into, its drain task, and
/// the tabs that asked for it.
pub struct Watch {
    pub reader: AgentId,
    pub task: TaskId,
    pub tabs: HashSet<String>,
}

/// One user's queue and what feeds it.
#[derive(Default)]
pub struct UserQ {
    pub seq: u64,
    pub q: VecDeque<Ev>,
    pub notify: Arc<Notify>,
    /// target id → its watch
    pub watches: HashMap<String, Watch>,
    /// tabs that subscribed to the state stream
    pub state_tabs: HashSet<String>,
    /// tab → when it last pulled
    pub seen: HashMap<String, Instant>,
}

impl UserQ {
    pub fn push(&mut self, v: Value, body: Option<Blob>) {
        self.seq += 1;
        self.q.push_back(Ev { seq: self.seq, v, body });
        while self.q.len() > KEEP {
            self.q.pop_front();
        }
        self.notify.notify_waiters();
    }

    /// The events after `after`: `Err(())` when some were dropped meanwhile.
    /// Up to the first one carrying a body, which is returned alone.
    pub fn after(&self, after: u64) -> Result<Vec<Ev>, ()> {
        if let Some(first) = self.q.front() {
            if after + 1 < first.seq {
                return Err(());
            }
        }
        let mut out = Vec::new();
        for e in self.q.iter().filter(|e| e.seq > after).take(crate::BATCH) {
            if e.body.is_some() {
                if out.is_empty() {
                    out.push(e.clone());
                }
                break;
            }
            out.push(e.clone());
        }
        Ok(out)
    }
}

/// Every user's queue.
#[derive(Default)]
pub struct Queues(pub Mutex<HashMap<String, UserQ>>);

impl Queues {
    pub fn with<T>(&self, user: &AgentId, f: impl FnOnce(&mut UserQ) -> T) -> T {
        let mut g = self.0.lock().expect("queues poisoned");
        f(g.entry(user.0.clone()).or_default())
    }

    pub fn push(&self, user: &AgentId, v: Value, body: Option<Blob>) {
        self.with(user, |q| q.push(v, body));
    }
}
