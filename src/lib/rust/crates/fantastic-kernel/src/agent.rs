//! Recursive `Agent` — the substrate's only node type.
//!
//! Identity + record + meta + children. `send`, `emit`, `watch`,
//! `cascade_delete` live in sibling modules (`send.rs`, `lifecycle.rs`)
//! and are added in sub-phase B.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

/// Stable identifier for an agent (newtype around `String`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    /// Borrow the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for AgentId {
    fn from(s: &str) -> Self {
        AgentId(s.to_string())
    }
}

impl From<String> for AgentId {
    fn from(s: String) -> Self {
        AgentId(s)
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Persistent fields written to `<root>/agent.yaml`. Matches the
/// shape Python persists, byte-for-byte.
///
/// `meta` flattens into the JSON object — `display_name`, `port`,
/// `root`, etc. all appear at the top level alongside `id` /
/// `handler_module` / `parent_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Agent id (unique tree-wide).
    pub id: String,
    /// Bundle handler key. `None` for the root and other bare agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler_module: Option<String>,
    /// All other key/value pairs.
    #[serde(flatten)]
    pub meta: Map<String, Value>,
    /// Parent id, omitted for the root.
    ///
    /// DECLARED LAST, so it SERIALIZES last — python writes `id`,
    /// `handler_module`, the meta, then `parent_id`, because structure wins: a
    /// `parent_id` that leaked into meta must not shadow where the agent
    /// actually sits (`P/kernel/_agent.py:510-527`). The port declared it third
    /// and every record on the wire and on disk disagreed with python's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// The recursive substrate node.
///
/// Stable across the substrate; reads are lock-free for hot paths
/// (children, watcher_ids guarded by tokio RwLock; meta + record by
/// std::sync::RwLock to keep them sync). Lifecycle methods land with
/// sub-phase B (`send.rs`, `lifecycle.rs`).
pub struct Agent {
    /// Identifier (e.g. `"kernel_state"`, `"file_abc123"`).
    pub id: AgentId,
    /// Bundle handler key, if any.
    pub handler_module: Option<String>,
    /// Parent id; `None` for the root.
    pub parent_id: Option<AgentId>,
    /// Arbitrary metadata persisted to agent.yaml (display_name, port, root, …).
    pub meta: std::sync::RwLock<Map<String, Value>>,
    /// Direct children, in CREATION ORDER.
    ///
    /// Python's `_children` is a plain dict, so the tree, `tree=ids` and every
    /// descendant walk read insertion order. A `DashMap` has no order at all,
    /// which is why the port sorted by id at every reader — and printed a tree
    /// python never prints. Reach it through [`Agent::add_child`],
    /// [`Agent::remove_child`] and [`Agent::child_ids`]; children counts are
    /// small and no site looks one up by key.
    pub(crate) children: std::sync::RwLock<Vec<Arc<Agent>>>,
    /// Synthetic ids (browser clients) + agent ids currently watching
    /// this agent's inbox. Updated by `watch` / `unwatch` in sub-phase B.
    #[allow(dead_code)]
    pub(crate) watcher_ids: RwLock<HashSet<AgentId>>,
    /// Where this agent's `agent.yaml` + `agents/` dir lives on disk.
    pub root_path: PathBuf,
    /// Skip disk writes when true (cli, repl, in-test fixtures).
    pub ephemeral: bool,
    /// Handler dispatches currently RUNNING on this agent — python's
    /// `_in_flight`. Bumped at dispatch start, dropped in the `finally`; it is
    /// the `backlog` every state event and the state snapshot report. Queue
    /// depth is a different number and was never what python answered.
    in_flight: std::sync::atomic::AtomicI64,
    /// `Some` for an agent on a CONNECTED PEER KERNEL (python's `RemoteTarget`):
    /// built by one send for `<guid>|<agent id>`, never registered in this tree,
    /// and dispatched through the peer's route instead of a handler here. Its
    /// `parent_id` is `None` like python's `parent = None`, so every path that
    /// reads "no parent" as "the root" must ask [`Agent::is_remote`] first.
    pub(crate) remote: Option<crate::peers::RemoteRef>,
}

/// Keys a RECORD may never carry: they name substrate wiring, not agent meta.
///
/// A hand-authored `root_path:` once cleared the whole tree and THEN raised —
/// the kernel was bricked by a field that looked like configuration. Refused at
/// the door, before any teardown, so a bad document cannot half-apply.
pub const RESERVED_RECORD_KEYS: [&str; 3] = ["root_path", "ctx", "parent"];

impl Agent {
    /// Construct a fresh agent. Caller is responsible for registering
    /// it in the kernel's `agents` map.
    ///
    /// `root_path` is the directory that holds (or will hold) this
    /// agent's `agent.yaml` + recursive `agents/` subtree.
    pub fn new(
        id: AgentId,
        handler_module: Option<String>,
        parent_id: Option<AgentId>,
        mut meta: Map<String, Value>,
        root_path: PathBuf,
        ephemeral: bool,
    ) -> Arc<Self> {
        // Structural authority baseline, stamped + MERGED on BOTH fields for EVERY
        // agent that passes through here — the seeded root, a created child, a
        // disk-loaded record alike (one funnel, so nothing lacks it). `can_call`
        // (who I may call) and `callable_by` (who may call me) both start as
        // {$self:*, $parent:*, $subtree:*}. It MERGES UNDER any explicit grant
        // already in `meta` (an explicit key overrides a baseline key; idempotent on
        // reload). Deny-all governs every edge NOT in the merged grant.
        for (field, base) in [
            ("can_call", crate::authz::default_can_call()),
            ("callable_by", crate::authz::default_callable_by()),
        ] {
            let mut merged = base;
            if let Some(Value::Object(explicit)) = meta.get(field).cloned() {
                for (k, v) in explicit {
                    merged.insert(k, v);
                }
            }
            meta.insert(field.into(), Value::Object(merged));
        }
        Arc::new(Self {
            id,
            handler_module,
            parent_id,
            meta: std::sync::RwLock::new(meta),
            children: std::sync::RwLock::new(Vec::new()),
            watcher_ids: RwLock::new(HashSet::new()),
            root_path,
            ephemeral,
            in_flight: std::sync::atomic::AtomicI64::new(0),
            remote: None,
        })
    }

    /// The target ONE send resolved `<guid>|<agent id>` to on a connected peer.
    /// No baseline: it is nobody's parent or child here, and its `callable_by` is
    /// python's empty map — whether it accepts a call is decided on its own
    /// kernel.
    pub(crate) fn remote(qualified: AgentId, remote: crate::peers::RemoteRef) -> Arc<Self> {
        Arc::new(Self {
            id: qualified,
            handler_module: None,
            parent_id: None,
            meta: std::sync::RwLock::new(crate::peers::remote_meta()),
            children: std::sync::RwLock::new(Vec::new()),
            watcher_ids: RwLock::new(HashSet::new()),
            root_path: PathBuf::new(),
            ephemeral: true,
            in_flight: std::sync::atomic::AtomicI64::new(0),
            remote: Some(remote),
        })
    }

    /// Is this an agent on a connected peer kernel (see [`Agent::remote`])?
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Dispatches currently running on this agent — the `backlog` of every state
    /// event and of `Kernel::state_snapshot`.
    pub fn in_flight(&self) -> i64 {
        self.in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Enter a dispatch (python's `self._in_flight += 1`).
    pub(crate) fn in_flight_enter(&self) {
        self.in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Leave a dispatch — python's `max(0, self._in_flight - 1)`, clamped so a
    /// stray drain can never take the counter negative.
    pub(crate) fn in_flight_leave(&self) {
        let _ = self.in_flight.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| Some((n - 1).max(0)),
        );
    }

    /// The persistable record snapshot — `id` + `handler_module` +
    /// `parent_id` + meta (flattened).
    pub fn record(&self) -> AgentRecord {
        let meta = self.meta.read().expect("meta lock poisoned").clone();
        AgentRecord {
            id: self.id.0.clone(),
            handler_module: self.handler_module.clone(),
            parent_id: self.parent_id.as_ref().map(|p| p.0.clone()),
            meta,
        }
    }

    /// Display name (best-effort, from meta).
    pub fn display_name(&self) -> Option<String> {
        self.meta
            .read()
            .expect("meta lock poisoned")
            .get("display_name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// `display_name or id` — python's truthiness: an EMPTY name is no name.
    /// The `name` of every added/removed state event and snapshot row.
    pub fn name_or_id(&self) -> String {
        self.display_name()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.id.0.clone())
    }

    /// Optional short `description` (best-effort, from meta) — a one-line
    /// "what this agent does", surfaced in every reflect. Set via
    /// create_agent / update_agent.
    pub fn description(&self) -> Option<String> {
        self.meta
            .read()
            .expect("meta lock poisoned")
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// Merge new key/value pairs into meta and return the updated record.
    pub fn update_meta(&self, patch: Map<String, Value>) -> AgentRecord {
        {
            let mut m = self.meta.write().expect("meta lock poisoned");
            for (k, v) in patch {
                m.insert(k, v);
            }
        }
        self.record()
    }

    /// Whether the record's `delete_lock` is TRUTHY. Substrate refuses
    /// cascade-delete on locked agents. Python truthiness
    /// (`P/kernel/_agent.py` `_find_locked_descendant`): `1`, `"yes"` or a
    /// non-empty list lock too — CLI `k=v` coercion hands over an int.
    pub fn is_delete_locked(&self) -> bool {
        self.meta
            .read()
            .expect("meta lock poisoned")
            .get("delete_lock")
            .is_some_and(fantastic_bundle::py_truthy)
    }

    /// Path of `agent.yaml` under `root_path`.
    pub fn agent_file(&self) -> PathBuf {
        self.root_path.join(crate::persistence::RECORD_FILE)
    }

    /// Does this agent own a child with the given id?
    pub fn has_child(&self, id: &AgentId) -> bool {
        self.children
            .read()
            .expect("children lock poisoned")
            .iter()
            .any(|c| c.id == *id)
    }

    /// Direct child count.
    pub fn child_count(&self) -> usize {
        self.children.read().expect("children lock poisoned").len()
    }

    /// Snapshot of direct child ids, in CREATION ORDER — python's dict order.
    pub fn child_ids(&self) -> Vec<AgentId> {
        self.children
            .read()
            .expect("children lock poisoned")
            .iter()
            .map(|c| c.id.clone())
            .collect()
    }

    /// Snapshot of the direct children themselves, in creation order.
    pub(crate) fn children_snapshot(&self) -> Vec<Arc<Agent>> {
        self.children
            .read()
            .expect("children lock poisoned")
            .clone()
    }

    /// Wire a child in. Re-wiring an id that is already there REPLACES it in
    /// place, keeping its position — python's `dict[k] = v`.
    pub(crate) fn add_child(&self, child: Arc<Agent>) {
        let mut kids = self.children.write().expect("children lock poisoned");
        match kids.iter().position(|c| c.id == child.id) {
            Some(i) => kids[i] = child,
            None => kids.push(child),
        }
    }

    /// Detach a child by id. Absent id → no-op.
    pub(crate) fn remove_child(&self, id: &AgentId) {
        self.children
            .write()
            .expect("children lock poisoned")
            .retain(|c| c.id != *id);
    }
}

#[cfg(test)]
mod tests;
