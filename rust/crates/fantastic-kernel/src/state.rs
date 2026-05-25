//! [`KernelState`] — the canonical, serializable snapshot of an
//! entire kernel.
//!
//! Both storage modes ([`crate::storage::StorageMode::Disk`] and
//! [`crate::storage::StorageMode::InMemory`]) hold their state in this
//! form. The only difference between modes is the *medium* — Disk
//! mode auto-flushes a `KernelState` to `<workdir>/.fantastic/state.json`
//! on every mutation, InMemory mode keeps it only in process memory
//! and exposes [`crate::Kernel::save`] / [`crate::Kernel::load`] for
//! on-demand snapshot / restore.
//!
//! Flat list rather than nested tree — `parent_id` on each record
//! encodes structure. Easier to mutate during reload (build agents
//! first, then wire parent/child relationships in a second pass) and
//! cheaper to diff between snapshots.

use crate::agent::AgentRecord;
use serde::{Deserialize, Serialize};

/// Current snapshot schema version. Bump when [`KernelState`]'s
/// on-the-wire shape breaks; old callers can still read snapshots
/// with `version <= CURRENT` and refuse `version > CURRENT`.
pub const CURRENT_VERSION: u32 = 1;

/// Canonical kernel state — every agent in a kernel, in serializable
/// form. Round-trips through [`crate::Kernel::save`] /
/// [`crate::Kernel::load`].
///
/// Wire shape (JSON):
///
/// ```json
/// {
///   "version": 1,
///   "agents": [
///     { "id": "core", "parent_id": null, "handler_module": null, "meta": {} },
///     { "id": "web_8888", "parent_id": "core", "handler_module": "web.tools",
///       "meta": { "port": 8888 } },
///     ...
///   ]
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KernelState {
    /// Schema version. See [`CURRENT_VERSION`].
    pub version: u32,
    /// Every agent in the kernel, flat. The root is the entry whose
    /// `parent_id` is `None`. Sorted by id (ASCII) for deterministic
    /// snapshots — byte-identical [`crate::Kernel::save_json`] output
    /// for equal states.
    pub agents: Vec<AgentRecord>,
}

impl KernelState {
    /// Empty snapshot at the current schema version. Mostly useful for
    /// tests and for constructing a kernel from no prior state.
    pub fn empty() -> Self {
        Self {
            version: CURRENT_VERSION,
            agents: Vec::new(),
        }
    }

    /// Return the root agent record (the entry whose `parent_id`
    /// is `None`), if any. There should be exactly one root in a
    /// well-formed snapshot; this returns the first match.
    pub fn root(&self) -> Option<&AgentRecord> {
        self.agents.iter().find(|a| a.parent_id.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn rec(id: &str, parent: Option<&str>) -> AgentRecord {
        AgentRecord {
            id: id.into(),
            handler_module: None,
            parent_id: parent.map(String::from),
            meta: Map::new(),
        }
    }

    #[test]
    fn empty_uses_current_version() {
        let s = KernelState::empty();
        assert_eq!(s.version, CURRENT_VERSION);
        assert!(s.agents.is_empty());
    }

    #[test]
    fn root_finds_orphan_entry() {
        let s = KernelState {
            version: 1,
            agents: vec![rec("core", None), rec("web", Some("core"))],
        };
        assert_eq!(s.root().map(|r| r.id.as_str()), Some("core"));
    }

    #[test]
    fn round_trips_through_json() {
        let s = KernelState {
            version: 1,
            agents: vec![rec("core", None), rec("a", Some("core"))],
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: KernelState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, parsed);
    }
}
