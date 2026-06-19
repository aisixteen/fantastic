//! Per-agent live-state map, keyed by [`AgentId`] in a process-global
//! `OnceLock`.
//!
//! Both runner transports keep a small amount of process-memory state
//! per agent (local = the spawned `fantastic` child handle; ssh = the
//! live `ssh -L` tunnel child + its pid). The map plumbing is
//! identical — only the stored `S` differs — so it lives here,
//! generic over the per-runner state type.
//!
//! `S` is created lazily via [`Default`] on first access. State is
//! shared across runner kinds only by id; a local agent's id never
//! collides with an ssh agent's in practice, and each runner owns its
//! own `RunnerMap<S>` static regardless.

use fantastic_kernel::AgentId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Process-global per-agent state map. Construct one as a `static`
/// with [`RunnerMap::new`]; store any `Send + Sync` per-agent `S`.
pub struct RunnerMap<S: Send + Sync>(OnceLock<Mutex<HashMap<AgentId, Arc<S>>>>);

impl<S: Send + Sync + Default> RunnerMap<S> {
    /// Construct an empty map (usable in `static` position).
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Arc<S>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("RunnerMap mutex poisoned")
    }

    /// Get (or lazily create via [`Default`]) the state for an agent.
    pub fn get_or_init_for(&self, id: &AgentId) -> Arc<S> {
        let mut map = self.map();
        if let Some(existing) = map.get(id) {
            return Arc::clone(existing);
        }
        let arc = Arc::new(S::default());
        map.insert(id.clone(), Arc::clone(&arc));
        arc
    }

    /// Drop an agent's state slot.
    pub fn remove(&self, id: &AgentId) {
        self.map().remove(id);
    }
}

impl<S: Send + Sync + Default> Default for RunnerMap<S> {
    fn default() -> Self {
        Self::new()
    }
}
