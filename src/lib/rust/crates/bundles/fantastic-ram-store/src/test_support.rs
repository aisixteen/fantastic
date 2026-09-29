//! TEST WIRING for an AI agent's memory store — the twin of python
//! `_testkit.ensure_memory` / `memory_meta`. An AI agent keeps its conversation in
//! the memory store its record names, and the gates between the two are EXPLICIT on
//! both ends (no defaults). Tests compose it the way an operator would: one
//! `ram_store` under the root, the agent granted `can_call` on it, the store's
//! `callable_by` opened for the agent. The store keys rows by the kernel's guid,
//! which a booted root mints; an unbooted test root gets one here.
//!
//! The caller registers [`crate::RamStoreBundle`] under [`crate::HANDLER_MODULE`].

use std::sync::Arc;

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

/// The shared test store's id.
pub const MEMORY: &str = "memory";

/// The shared test memory store under the root (created on first use); the root
/// gets a guid if it was never booted. Returns the store id.
///
/// # Panics
/// When the kernel has no root, or the store cannot be created.
pub async fn ensure_memory(kernel: &Arc<Kernel>) -> AgentId {
    let root = kernel
        .root()
        .expect("ensure_memory: the kernel has no root");
    let has_guid = root
        .meta
        .read()
        .expect("root meta poisoned")
        .get("guid")
        .and_then(Value::as_str)
        .is_some_and(|g| !g.is_empty());
    if !has_guid {
        let mut patch = Map::new();
        patch.insert("guid".to_string(), json!("test-guid"));
        kernel.update(&root.id, patch);
    }
    let store = AgentId::from(MEMORY);
    if !kernel.agents.contains_key(&store) {
        let r = kernel
            .send(vec![Item::new(
                root.id.clone(),
                json!({
                    "type": "create_agent",
                    "parent_id": root.id.0,
                    "handler_module": crate::HANDLER_MODULE,
                    "id": MEMORY,
                    "operator": true,
                    "admins": ["kernel"],
                }),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], json!(MEMORY), "ensure_memory: {r}");
    }
    store
}

/// `create_agent` meta for AI agent `ai_id`: `memory_store_id` + `can_call` on the
/// store (merged into `can_call`). Creates the store on first use and opens its
/// `callable_by` for `ai_id`.
pub async fn memory_meta(kernel: &Arc<Kernel>, ai_id: &str, can_call: Value) -> Map<String, Value> {
    let store = ensure_memory(kernel).await;
    let mut cb = kernel
        .agents
        .get(&store)
        .and_then(|a| {
            a.meta
                .read()
                .expect("store meta poisoned")
                .get("callable_by")
                .and_then(Value::as_object)
                .cloned()
        })
        .unwrap_or_default();
    cb.insert(ai_id.to_string(), json!("*"));
    let mut patch = Map::new();
    patch.insert("callable_by".to_string(), Value::Object(cb));
    kernel.update(&store, patch);
    let mut grants = can_call.as_object().cloned().unwrap_or_default();
    grants.insert(MEMORY.to_string(), json!("*"));
    let mut meta = Map::new();
    meta.insert("memory_store_id".to_string(), json!(MEMORY));
    meta.insert("can_call".to_string(), Value::Object(grants));
    meta
}
