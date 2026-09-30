//! THE CANVAS'S REACH — god mode, by grants (owner, 2026-09-30: "godmode";
//! the kernel: grants only, no kernel change).
//!
//! The canvas replaces the terminal shell's browser View, so its socket leg
//! (`canvas_ws`) reaches every agent the way the shell does. The shell is the
//! operator (ungated); the leg is not, so THIS CLI — the in-proc operator —
//! writes BOTH edges for every agent:
//!
//! - the agent's `callable_by` names `canvas_ws` with [`VERBS`];
//! - `canvas_ws`'s `can_call` names the agent with the same list (no `*` id key
//!   exists by design: each target, by id, `kernel` included).
//!
//! `*` never covers a PRIVILEGED verb, so each one is named. Done for every agent
//! at boot, and again for each agent the state stream reports `added` (made by
//! an AI, by exec, loaded from disk). The leg's `ingress_rule` still decides who
//! may open frames on it.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// The canvas leg's id.
pub const LEG: &str = crate::kern::web::CANVAS_WS_ID;

/// Every verb: `*`, and each privileged verb by name.
pub const VERBS: [&str; 17] = [
    "*",
    "create_agent",
    "delete_agent",
    "update_agent",
    "state_subscribe",
    "allow",
    "disallow",
    "ps",
    "cancel",
    "ledger",
    "ack_deadletter",
    "kernel_start",
    "kernel_stop",
    "shutdown_agents",
    "persist_record",
    "forget_record",
    "load_tree",
];

fn field(h: &Host, id: &str, f: &str) -> Map<String, Value> {
    h.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .and_then(|a| a.value().record().meta.get(f).and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// Grant the leg every verb on `ids` — both edges. The leg's side is one write.
pub async fn grant(h: &Host, ids: &[String]) -> Result<(), String> {
    let all = json!(VERBS);
    let mut cc = field(h, LEG, "can_call");
    let mut leg_changed = false;
    for id in ids {
        if id == LEG || !h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id.as_str())) {
            continue;
        }
        let mut cb = field(h, id, "callable_by");
        if cb.get(LEG) != Some(&all) {
            cb.insert(LEG.into(), all.clone());
            let r = h.operator("kernel", json!({"type": "update_agent", "id": id, "callable_by": cb})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("canvas reach: `{id}`.callable_by: {e}"));
            }
        }
        if cc.get(id.as_str()) != Some(&all) {
            cc.insert(id.clone(), all.clone());
            leg_changed = true;
        }
    }
    // the kernel's own verbs, sent to the reserved `kernel` address
    if cc.get("kernel") != Some(&all) {
        cc.insert("kernel".into(), all.clone());
        leg_changed = true;
    }
    if leg_changed {
        let r = h.operator("kernel", json!({"type": "update_agent", "id": LEG, "can_call": cc})).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("canvas reach: `{LEG}`.can_call: {e}"));
        }
    }
    Ok(())
}

/// Every agent now, then each one the state stream reports `added`.
pub async fn start(h: &Host) -> Result<(), String> {
    let ids: Vec<String> = h.kernel.agents.iter().map(|e| e.key().0.clone()).collect();
    grant(h, &ids).await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let sub: fantastic_kernel::StateSubscriber = Arc::new(move |v: &Value, _body: Option<&(String, Vec<u8>)>| {
        if v.get("kind").and_then(Value::as_str) == Some("added") {
            if let Some(id) = v.get("agent_id").and_then(Value::as_str) {
                let _ = tx.send(id.to_string());
            }
        }
    });
    h.kernel.add_state_subscriber(sub);
    let host = Host { kernel: Arc::clone(&h.kernel), cfg: h.cfg.clone(), root: h.root.clone() };
    tokio::spawn(async move {
        while let Some(id) = rx.recv().await {
            let mut batch = vec![id];
            while let Ok(more) = rx.try_recv() {
                batch.push(more);
            }
            if let Err(e) = grant(&host, &batch).await {
                tracing::warn!("{e}");
            }
        }
    });
    Ok(())
}
