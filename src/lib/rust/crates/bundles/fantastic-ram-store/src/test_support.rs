//! TEST WIRING for an AI agent's memory store — the twin of python
//! `_testkit.ensure_memory` / `memory_meta`. An AI agent keeps its conversation in
//! the memory store its record names, and the agent reaches it only by a line in
//! its own `can` (no defaults). Tests compose it the way an operator would: one
//! `ram_store` under the root, the agent holding `memory: {call: ["*"]}`. The store
//! keys rows by the kernel's guid, which a booted root mints; an unbooted test root
//! gets one here (the operator's `update_agent`).
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
        .record().meta
        .get("guid")
        .and_then(Value::as_str)
        .is_some_and(|g| !g.is_empty());
    if !has_guid {
        let r = kernel
            .operator_send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "update_agent", "id": root.id.0, "guid": "test-guid"}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["updated"], json!(true), "ensure_memory: {r}");
    }
    let store = AgentId::from(MEMORY);
    if !kernel.exists(store.as_str()) {
        let r = kernel
            .operator_send(vec![Item::new(
                root.id.clone(),
                json!({
                    "type": "create_agent",
                    "parent_id": root.id.0,
                    "handler_module": crate::HANDLER_MODULE,
                    "id": MEMORY,
                }),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], json!(MEMORY), "ensure_memory: {r}");
    }
    store
}

/// `create_agent` meta for an AI agent: `memory_store_id` + a `call *` line on the
/// store, merged into the lines of `can` (the agent's other lines). Creates the
/// store on first use.
pub async fn memory_meta(kernel: &Arc<Kernel>, _ai_id: &str, can: Value) -> Map<String, Value> {
    ensure_memory(kernel).await;
    let mut lines = can.as_object().cloned().unwrap_or_default();
    lines.insert(MEMORY.to_string(), json!({"call": ["*"]}));
    let mut meta = Map::new();
    meta.insert("memory_store_id".to_string(), json!(MEMORY));
    meta.insert("can".to_string(), Value::Object(lines));
    meta
}
