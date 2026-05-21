//! Shared kernel context — flat agent index, inbox queues, bundle
//! registry, root pointer.
//!
//! The [`Kernel`] is `Arc`-cloneable and `Send + Sync` so handler
//! closures can capture it without contention. Concurrent paths use
//! lock-free `DashMap`s for the agent + inbox lookups; the root
//! pointer is an `ArcSwap` so swaps during boot are observable
//! atomically by every reader.

use crate::agent::{Agent, AgentId};
use crate::bundle::BundleRegistry;
use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

/// Bounded inbox capacity per agent — matches Python's `INBOX_BOUND`.
/// 256 fits a chat-paced workload comfortably and bounds memory on a
/// runaway emitter. Tunable via [`Kernel::new_with_inbox_bound`].
pub const DEFAULT_INBOX_BOUND: usize = 256;

/// A subscriber callback fired on every state event (`send` / `emit`
/// / `removed` / etc.). Synchronous, called inline — keep it cheap;
/// off-thread the work if it might block.
pub type StateSubscriber = Arc<dyn Fn(&Value) + Send + Sync>;

/// Opaque token returned by [`Kernel::add_state_subscriber`]; pass to
/// [`Kernel::remove_state_subscriber`] to detach.
///
/// The inner `u64` is `pub` so foreign-language bindings (e.g.
/// fantastic-uniffi for Swift) can round-trip the value through a
/// primitive type. Treat it as opaque from Rust callers — its
/// numeric value carries no meaning beyond "token issued by this
/// kernel".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberToken(pub u64);

/// Shared kernel context.
pub struct Kernel {
    /// Flat global id → Agent. The only routing table.
    pub agents: DashMap<AgentId, Arc<Agent>>,
    /// Per-id inbox (agents + synthetic browser client ids).
    pub inboxes: DashMap<AgentId, mpsc::Sender<Value>>,
    /// Active state-event taps (telemetry, debugging). Keyed by token
    /// for clean detach.
    state_subscribers: RwLock<Vec<(SubscriberToken, StateSubscriber)>>,
    /// Monotonic counter for [`SubscriberToken`].
    next_subscriber: AtomicU64,
    /// Compile-time bundle registry — caller fills before bootstrap.
    pub bundles: BundleRegistry,
    /// The root agent (`id="core"` by convention). Set during bootstrap.
    pub root: ArcSwapOption<Agent>,
    /// Per-inbox bound; same value applied to every channel.
    pub inbox_bound: usize,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    /// Empty kernel — no agents, no root, default inbox bound.
    pub fn new() -> Self {
        Self::new_with_inbox_bound(DEFAULT_INBOX_BOUND)
    }

    /// Empty kernel with a custom inbox bound. Use for testing
    /// back-pressure paths.
    pub fn new_with_inbox_bound(bound: usize) -> Self {
        Self {
            agents: DashMap::new(),
            inboxes: DashMap::new(),
            state_subscribers: RwLock::new(Vec::new()),
            next_subscriber: AtomicU64::new(1),
            bundles: BundleRegistry::new(),
            root: ArcSwapOption::const_empty(),
            inbox_bound: bound,
        }
    }

    /// Register an agent in the flat index + auto-vivify its inbox.
    /// Returns the bound channel receiver so the caller can drain it
    /// (typically a fanout loop or a per-bundle handler task).
    pub fn register(&self, agent: Arc<Agent>) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(self.inbox_bound);
        self.inboxes.insert(agent.id.clone(), tx);
        self.agents.insert(agent.id.clone(), agent);
        rx
    }

    /// Unregister an agent + drop its inbox. Caller has already run
    /// the bundle's `on_delete` hook + rmtreed the dir.
    pub fn unregister(&self, id: &AgentId) {
        self.inboxes.remove(id);
        self.agents.remove(id);
    }

    /// Cheap clone of the current root pointer (`None` until
    /// bootstrap completes).
    pub fn root(&self) -> Option<Arc<Agent>> {
        self.root.load_full()
    }

    /// Swap the root pointer atomically. Bootstrap calls this once.
    pub fn set_root(&self, root: Arc<Agent>) {
        self.root.store(Some(root));
    }

    /// Push a state event to every subscriber. Keep callbacks short —
    /// they run on the dispatching task.
    pub fn publish_state(&self, event: &Value) {
        let subs = self.state_subscribers.read().expect("state subs poisoned");
        for (_token, cb) in subs.iter() {
            cb(event);
        }
    }

    /// Register a state-event tap. Returns a token the caller can
    /// pass to [`Self::remove_state_subscriber`] to detach.
    pub fn add_state_subscriber(&self, cb: StateSubscriber) -> SubscriberToken {
        let token = SubscriberToken(self.next_subscriber.fetch_add(1, Ordering::SeqCst));
        self.state_subscribers
            .write()
            .expect("state subs poisoned")
            .push((token, cb));
        token
    }

    /// Detach a previously-registered subscriber. No-op if `token`
    /// isn't (or no longer is) registered.
    pub fn remove_state_subscriber(&self, token: SubscriberToken) {
        self.state_subscribers
            .write()
            .expect("state subs poisoned")
            .retain(|(t, _)| *t != token);
    }

    /// Synchronous read of every loaded agent's identity + display
    /// name. Mirrors Python's `Kernel.state_snapshot` — used by new
    /// `state_subscribe` subscribers to bootstrap their agent view
    /// before the first event arrives. No queue puts, no fanout.
    ///
    /// Each entry: `{agent_id, name, backlog: 0}`. `backlog` is the
    /// number of in-flight handler dispatches; Rust doesn't track
    /// this counter today, so we report 0 (consumers that rely on
    /// it for ordering can recompute from observed traffic). Matches
    /// Python's wire shape so existing telemetry pane consumers work.
    pub fn state_snapshot(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .agents
            .iter()
            .map(|entry| {
                let a = entry.value();
                let name = a.display_name().unwrap_or_else(|| a.id.0.clone());
                serde_json::json!({
                    "agent_id": a.id.0,
                    "name": name,
                    "backlog": 0,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            a.get("agent_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("agent_id").and_then(Value::as_str).unwrap_or(""))
        });
        out
    }
}

#[cfg(test)]
mod tests;
