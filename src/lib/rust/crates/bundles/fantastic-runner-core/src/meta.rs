//! Shared agent-record (`meta`) snapshot helpers.
//!
//! Both runner transports read their configuration from the agent's
//! persisted record (`meta`). These helpers snapshot the record under
//! the kernel lock and pull typed fields out of it — identical logic
//! that previously lived (copy-pasted) in each runner crate.

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{Map, Value};
use std::sync::Arc;

/// Snapshot an agent's `meta` map under the kernel lock. Returns an
/// empty map if the agent is unknown.
pub fn snapshot_meta(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a.meta.read().expect("meta poisoned").clone(),
        None => Map::new(),
    }
}

/// A string field from the record, or `None` if absent / non-string.
pub fn meta_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key).and_then(Value::as_str)
}

/// A `u16` port field from the record. `None` unless present and in
/// `1..=u16::MAX`.
pub fn meta_u16(meta: &Map<String, Value>, key: &str) -> Option<u16> {
    meta.get(key)
        .and_then(Value::as_u64)
        .filter(|p| *p > 0 && *p <= u16::MAX as u64)
        .map(|p| p as u16)
}
