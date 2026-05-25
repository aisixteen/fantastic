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

use dashmap::DashMap;
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

/// Persistent fields written to `<root>/agent.json`. Matches the
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
    /// Parent id, omitted for the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// All other key/value pairs.
    #[serde(flatten)]
    pub meta: Map<String, Value>,
}

/// The recursive substrate node.
///
/// Stable across the substrate; reads are lock-free for hot paths
/// (children, watcher_ids guarded by tokio RwLock; meta + record by
/// std::sync::RwLock to keep them sync). Lifecycle methods land with
/// sub-phase B (`send.rs`, `lifecycle.rs`).
pub struct Agent {
    /// Identifier (e.g. `"core"`, `"file_abc123"`).
    pub id: AgentId,
    /// Bundle handler key, if any.
    pub handler_module: Option<String>,
    /// Parent id; `None` for the root.
    pub parent_id: Option<AgentId>,
    /// Arbitrary metadata persisted to agent.json (display_name, port, root, …).
    pub meta: std::sync::RwLock<Map<String, Value>>,
    /// Direct children by id.
    pub(crate) children: DashMap<AgentId, Arc<Agent>>,
    /// Synthetic ids (browser clients) + agent ids currently watching
    /// this agent's inbox. Updated by `watch` / `unwatch` in sub-phase B.
    #[allow(dead_code)]
    pub(crate) watcher_ids: RwLock<HashSet<AgentId>>,
    /// Where this agent's `agent.json` + `agents/` dir lives on disk.
    pub root_path: PathBuf,
    /// Skip disk writes when true (cli, repl, in-test fixtures).
    pub ephemeral: bool,
}

impl Agent {
    /// Construct a fresh agent. Caller is responsible for registering
    /// it in the kernel's `agents` map.
    ///
    /// `root_path` is the directory that holds (or will hold) this
    /// agent's `agent.json` + recursive `agents/` subtree.
    pub fn new(
        id: AgentId,
        handler_module: Option<String>,
        parent_id: Option<AgentId>,
        meta: Map<String, Value>,
        root_path: PathBuf,
        ephemeral: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            handler_module,
            parent_id,
            meta: std::sync::RwLock::new(meta),
            children: DashMap::new(),
            watcher_ids: RwLock::new(HashSet::new()),
            root_path,
            ephemeral,
        })
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

    /// Whether the record carries `delete_lock: true`. Substrate
    /// refuses cascade-delete on locked agents.
    pub fn is_delete_locked(&self) -> bool {
        self.meta
            .read()
            .expect("meta lock poisoned")
            .get("delete_lock")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Path of `agent.json` under `root_path`.
    pub fn agent_file(&self) -> PathBuf {
        self.root_path.join("agent.json")
    }

    /// Path of the `agents/` subdir under `root_path` (children live here).
    pub fn children_dir(&self) -> PathBuf {
        self.root_path.join("agents")
    }

    /// Path of `readme.md` under `root_path` (seeded from the bundle).
    pub fn readme_file(&self) -> PathBuf {
        self.root_path.join("readme.md")
    }

    /// Does this agent own a child with the given id?
    pub fn has_child(&self, id: &AgentId) -> bool {
        self.children.contains_key(id)
    }

    /// Direct child count.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Snapshot of direct child ids (sorted for deterministic iteration).
    pub fn child_ids(&self) -> Vec<AgentId> {
        let mut ids: Vec<AgentId> = self
            .children
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        ids
    }
}

#[cfg(test)]
mod tests;
