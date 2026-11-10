//! "Is this an AI agent?" — the one rule, from the kernel's own tables.
//!
//! An AI agent is one whose handler's record schema names a `memory_store_id`
//! (the field that gives it a conversation store) and whose record carries one.

use fantastic_kernel::{AgentId, Kernel};

/// Whether `handler` makes an AI agent: its bundle's record schema names a
/// `memory_store_id` field. `false` for a handler no bundle answers to.
#[must_use]
pub fn is_ai_handler(kernel: &Kernel, handler: &str) -> bool {
    kernel
        .bundles
        .catalog()
        .find(|(m, _)| *m == handler)
        .and_then(|(_, b)| b.record_schema())
        .is_some_and(|s| {
            s["fields"]
                .as_array()
                .is_some_and(|a| a.iter().any(|f| f["name"] == "memory_store_id"))
        })
}

/// Whether the agent `id` is an AI agent: its record holds a string
/// `memory_store_id` and its handler is an AI handler ([`is_ai_handler`]).
/// `false` for an id that is not in the tree.
#[must_use]
pub fn is_ai(kernel: &Kernel, id: &str) -> bool {
    let Some(a) = kernel.agents.get(&AgentId::from(id)) else {
        return false;
    };
    let r = a.value().record();
    let Some(hm) = r.handler_module.as_deref() else {
        return false;
    };
    r.meta
        .get("memory_store_id")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && is_ai_handler(kernel, hm)
}

#[cfg(test)]
#[path = "ai_check/tests.rs"]
mod tests;
