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
use crate::errors::{KernelError, KernelResult};
use crate::state::{KernelState, CURRENT_VERSION};
use crate::storage::StorageMode;
use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use serde_json::Value;
use std::path::PathBuf;
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
/// The inner `u64` is `pub` so embedding consumers can round-trip the
/// value through a primitive type (e.g. a JSON string) across a
/// process boundary. Treat it as opaque from Rust callers — its
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
    /// State medium. [`StorageMode::Disk`] auto-flushes to
    /// `<workdir>/.fantastic/state.json` on every mutation;
    /// [`StorageMode::InMemory`] never touches the filesystem. See
    /// [`Self::save`] / [`Self::load`] for the on-demand API both
    /// modes share.
    pub storage: StorageMode,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    /// Empty kernel — no agents, no root, default inbox bound,
    /// [`StorageMode::InMemory`]. The `Disk` mode is opt-in via
    /// [`Self::with_storage`]; the no-arg ctor keeps the existing
    /// per-test default (these tests never persist).
    pub fn new() -> Self {
        Self::with_storage(StorageMode::InMemory)
    }

    /// Empty kernel with a specific [`StorageMode`]. Used by
    /// [`crate::bootstrap::bootstrap`] to pick `Disk` or `InMemory`
    /// based on caller options.
    pub fn with_storage(storage: StorageMode) -> Self {
        Self::with_storage_and_inbox(storage, DEFAULT_INBOX_BOUND)
    }

    /// Empty kernel with a custom inbox bound. Use for testing
    /// back-pressure paths.
    pub fn new_with_inbox_bound(bound: usize) -> Self {
        Self::with_storage_and_inbox(StorageMode::InMemory, bound)
    }

    /// Empty kernel with both a [`StorageMode`] and a custom inbox
    /// bound — the underlying ctor every other variant funnels into.
    pub fn with_storage_and_inbox(storage: StorageMode, bound: usize) -> Self {
        Self {
            agents: DashMap::new(),
            inboxes: DashMap::new(),
            state_subscribers: RwLock::new(Vec::new()),
            next_subscriber: AtomicU64::new(1),
            bundles: BundleRegistry::new(),
            root: ArcSwapOption::const_empty(),
            inbox_bound: bound,
            storage,
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

    // ── save / load — the foundation primitive both storage modes share ──

    /// Snapshot the live kernel state as a [`KernelState`] value.
    /// Both [`StorageMode::Disk`] and [`StorageMode::InMemory`]
    /// kernels produce equal output for equal in-memory state — the
    /// medium difference is irrelevant here.
    ///
    /// Pure read; iterates `self.agents`, builds the snapshot in
    /// id-sorted order so [`Self::save_json`] is byte-deterministic.
    /// Ephemeral agents are skipped (they're per-process composition,
    /// never round-trip through a snapshot).
    pub fn save(&self) -> KernelState {
        let mut agents: Vec<_> = self
            .agents
            .iter()
            .filter(|e| !e.value().ephemeral)
            .map(|e| e.value().record())
            .collect();
        agents.sort_by(|a, b| a.id.cmp(&b.id));
        KernelState {
            version: CURRENT_VERSION,
            agents,
        }
    }

    /// [`Self::save`] serialized to JSON. Used by [`Self::maybe_flush`]
    /// (Disk auto-sync) and by the embedding API.
    /// Output is byte-deterministic for equal kernel states.
    pub fn save_json(&self) -> String {
        serde_json::to_string(&self.save()).expect("KernelState is JSON-serializable")
    }

    /// Replace this kernel's agent tree with `state`. Drops every
    /// currently-registered agent (including their inboxes), then
    /// rebuilds from the snapshot.
    ///
    /// Does NOT fire `boot` hooks — matches the existing
    /// `persistence::load_children` semantics. The bootstrap caller
    /// dispatches `{type:"boot"}` to whichever agents need it
    /// (typically the web agent, or a bundle that boots paired agents,
    /// etc.). For brain-kernel-style snapshot reload, the consumer
    /// can iterate `kernel.agents` post-load and fire boot themselves
    /// if needed.
    ///
    /// Weak-load: agents whose `handler_module` isn't in this
    /// kernel's [`BundleRegistry`] are logged + skipped along with
    /// their entire subtree (a parent's missing bundle implies all
    /// its descendants are unreachable). The record is dropped from
    /// the loaded set; subsequent [`Self::save`] won't see it.
    ///
    /// Errors:
    /// - [`KernelError::InvalidSnapshot`] if `state.version` exceeds
    ///   [`CURRENT_VERSION`], if the snapshot has no root, or if any
    ///   record's `parent_id` doesn't point at a record in the
    ///   snapshot.
    pub fn load(&self, state: KernelState) -> KernelResult<()> {
        if state.version > CURRENT_VERSION {
            return Err(KernelError::InvalidSnapshot(format!(
                "snapshot version {} exceeds this kernel's max ({})",
                state.version, CURRENT_VERSION
            )));
        }

        // Validate: every parent_id must point at a record in the
        // snapshot, exactly one record must have parent_id == None
        // (the root), every id must be unique.
        let mut roots = 0;
        let mut ids_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for rec in &state.agents {
            if !ids_seen.insert(rec.id.as_str()) {
                return Err(KernelError::InvalidSnapshot(format!(
                    "duplicate agent id {:?} in snapshot",
                    rec.id
                )));
            }
            if rec.parent_id.is_none() {
                roots += 1;
            }
        }
        if roots == 0 {
            return Err(KernelError::InvalidSnapshot(
                "snapshot has no root (no record with parent_id == null)".to_string(),
            ));
        }
        if roots > 1 {
            return Err(KernelError::InvalidSnapshot(format!(
                "snapshot has {roots} roots; expected exactly one"
            )));
        }
        for rec in &state.agents {
            if let Some(pid) = &rec.parent_id {
                if !ids_seen.contains(pid.as_str()) {
                    return Err(KernelError::InvalidSnapshot(format!(
                        "agent {:?} parent {:?} not in snapshot",
                        rec.id, pid
                    )));
                }
            }
        }

        // Drop the current tree. Inboxes close as their senders are
        // dropped from the DashMap (subscribers see the channel
        // closing and clean up).
        self.agents.clear();
        self.inboxes.clear();
        self.root.store(None);

        // Compute the per-agent on-disk root_path. Disk mode roots at
        // `<workdir>/.fantastic/`; children nest under `agents/<id>/`.
        // InMemory mode uses an empty PathBuf (never read).
        let disk_root: Option<PathBuf> = self.storage.workdir().map(|w| w.join(".fantastic"));

        // Two-pass: build all Agent structs first (parent_id strings
        // only), then wire parent/child Arc references. The
        // weak-load skip happens in the first pass — a record whose
        // handler_module isn't registered is dropped, along with any
        // descendant whose ancestor chain hits the dropped record.
        let mut by_id: std::collections::HashMap<String, Arc<Agent>> =
            std::collections::HashMap::new();
        let mut skip_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Topologically order: roots first, then BFS by ancestor depth.
        // Simpler approach: iterate; if a record's parent_id is a
        // skipped id, skip this one too. Repeat until no progress.
        // For sane snapshots (≤ a few thousand records) the cost is
        // O(N * depth), negligible.
        let mut pending: Vec<&crate::agent::AgentRecord> = state.agents.iter().collect();
        // Sort so parents always precede children when parent_id is
        // None or refers to an earlier record. Simple stable sort by
        // (parent_depth, id) — implemented by a multi-pass walk.
        let mut made_progress = true;
        while made_progress && !pending.is_empty() {
            made_progress = false;
            let mut next: Vec<&crate::agent::AgentRecord> = Vec::new();
            for rec in pending.drain(..) {
                let parent_id = rec.parent_id.clone();
                if let Some(pid) = parent_id.as_ref() {
                    if skip_ids.contains(pid) {
                        // Cascade skip — parent was dropped (weak-load).
                        skip_ids.insert(rec.id.clone());
                        tracing::warn!(
                            agent = %rec.id,
                            ancestor = %pid,
                            "skipping agent during load (ancestor dropped)",
                        );
                        made_progress = true;
                        continue;
                    }
                    if !by_id.contains_key(pid) {
                        // Parent not yet processed — re-queue.
                        next.push(rec);
                        continue;
                    }
                }

                // Weak-load: refuse this record if its handler_module
                // isn't registered. Root has no handler_module so
                // always loads.
                if let Some(hm) = rec.handler_module.as_deref() {
                    if self.bundles.get(hm).is_none() {
                        eprintln!(
                            "[kernel] skipping agent {}: bundle {} not installed in this runtime",
                            rec.id, hm,
                        );
                        skip_ids.insert(rec.id.clone());
                        made_progress = true;
                        continue;
                    }
                }

                // Construct the root_path. Disk mode: agent path is
                // `<workdir>/.fantastic[/agents/<id>]*` walking up the
                // parent chain. InMemory mode: empty PathBuf.
                let root_path = match (&disk_root, rec.parent_id.as_ref()) {
                    // Disk root.
                    (Some(disk), None) => disk.clone(),
                    // Disk child — reuse parent's already-composed
                    // root_path so we get `<disk>/agents/<id1>/agents/<id2>/...`.
                    (Some(_), Some(pid)) => by_id
                        .get(pid)
                        .expect("parent processed before child")
                        .children_dir()
                        .join(&rec.id),
                    // InMemory — root_path is never read; empty sentinel.
                    (None, _) => PathBuf::new(),
                };

                let agent = Agent::new(
                    AgentId::from(rec.id.as_str()),
                    rec.handler_module.clone(),
                    rec.parent_id.as_ref().map(|s| AgentId::from(s.as_str())),
                    rec.meta.clone(),
                    root_path,
                    false, // not ephemeral — ephemeral agents are never in a snapshot
                );
                let _rx = self.register(Arc::clone(&agent));
                by_id.insert(rec.id.clone(), Arc::clone(&agent));

                // Wire parent → child Arc.
                if let Some(pid) = &rec.parent_id {
                    if let Some(parent_arc) = by_id.get(pid) {
                        parent_arc
                            .children
                            .insert(agent.id.clone(), Arc::clone(&agent));
                    }
                } else {
                    self.set_root(Arc::clone(&agent));
                }

                made_progress = true;
            }
            pending = next;
        }

        if !pending.is_empty() {
            // Cycle or orphan whose parent never appeared (shouldn't
            // happen — validation above caught this).
            return Err(KernelError::InvalidSnapshot(format!(
                "could not resolve {} agents during load (cycle?)",
                pending.len()
            )));
        }

        Ok(())
    }

    /// JSON-string form of [`Self::load`]. Parses then loads. Used by
    /// the embedding API and the Disk-mode boot path.
    pub fn load_json(&self, json: &str) -> KernelResult<()> {
        let state: KernelState = serde_json::from_str(json)
            .map_err(|e| KernelError::InvalidSnapshot(format!("parse: {e}")))?;
        self.load(state)
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
