//! THE CANVAS'S REACH — god mode, by grants (owner, 2026-09-30: "godmode";
//! the kernel: grants only, no kernel change).
//!
//! The canvas replaces the terminal shell's browser View, so its socket leg
//! (`canvas_ws`) reaches every agent the way the shell does. The shell is the
//! operator (ungated); the leg is not, so THIS CLI — the in-proc operator —
//! writes BOTH edges for every agent:
//!
//! - the agent's `callable_by` accepts `$subtree:canvas_ws` with [`VERBS`] — the
//!   leg AND everything under it;
//! - the leg's `can_call` names the agent with the same list, and so does every
//!   AI CELL under the leg's (the canvas's control plane, owner 2026-09-30) — a
//!   heritable entry cannot serve both the leg and its descendants, so each
//!   holder carries its own (no `*` id key exists by design: each target, by id,
//!   `kernel` included).
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

/// Who ACCEPTS the canvas side on a target: the leg and everything under it (the
/// AI cells — the canvas's control plane — live there).
fn accepts() -> String {
    format!("{}{LEG}", SUBTREE)
}
const SUBTREE: &str = "$subtree:";

/// The HOLDERS of the reach: the leg, and every AI agent under it (an agent that
/// names a memory store — `ai_cells` spawns them as the leg's descendants).
fn holders(h: &Host) -> Vec<String> {
    let under_leg = |id: &str| -> bool {
        let mut cur = h.kernel.agents.get(&fantastic_kernel::AgentId::from(id)).and_then(|a| a.value().record().parent_id);
        while let Some(p) = cur {
            if p == LEG {
                return true;
            }
            cur = h.kernel.agents.get(&fantastic_kernel::AgentId::from(p.as_str())).and_then(|a| a.value().record().parent_id);
        }
        false
    };
    let mut v = vec![LEG.to_string()];
    v.extend(
        h.kernel
            .agents
            .iter()
            .filter(|e| e.value().record().meta.get("memory_store_id").and_then(Value::as_str).is_some())
            .map(|e| e.key().0.clone())
            .filter(|id| under_leg(id)),
    );
    v
}

/// Grant every holder every verb on `ids` — both edges: each target's
/// `callable_by` accepts `$subtree:canvas_ws`, and each holder's `can_call`
/// names the target (and `kernel`). One write per changed record.
pub async fn grant(h: &Host, ids: &[String]) -> Result<(), String> {
    let all = json!(VERBS);
    let sub = accepts();
    for id in ids {
        if !h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id.as_str())) {
            continue;
        }
        let mut cb = field(h, id, "callable_by");
        // the bare leg key is covered by the subtree (and was the old form)
        let stale = cb.remove(LEG).is_some();
        if stale || cb.get(&sub) != Some(&all) {
            cb.insert(sub.clone(), all.clone());
            let r = h.operator("kernel", json!({"type": "update_agent", "id": id, "callable_by": cb})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("canvas reach: `{id}`.callable_by: {e}"));
            }
        }
    }
    for holder in holders(h) {
        let mut cc = field(h, &holder, "can_call");
        let mut changed = false;
        for id in ids.iter().map(String::as_str).chain(["kernel"]) {
            if id == holder || (id != "kernel" && !h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id))) {
                continue;
            }
            if cc.get(id) != Some(&all) {
                cc.insert(id.to_string(), all.clone());
                changed = true;
            }
        }
        if changed {
            let r = h.operator("kernel", json!({"type": "update_agent", "id": holder, "can_call": cc})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("canvas reach: `{holder}`.can_call: {e}"));
            }
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
            // a new AI cell is a new HOLDER: it reaches every agent, not just the batch
            let holders_now = holders(&host);
            let batch = if batch.iter().any(|b| holders_now.contains(b)) {
                host.kernel.agents.iter().map(|e| e.key().0.clone()).collect()
            } else {
                batch
            };
            if let Err(e) = grant(&host, &batch).await {
                tracing::warn!("{e}");
            }
        }
    });
    Ok(())
}
