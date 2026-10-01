//! THE LEGS' REACH — god mode, by grants (owner, 2026-09-30: "godmode"; for
//! the MCP leg too, 2026-10-01: "* grants … it can shape grants later"; the
//! kernel: grants only, no kernel change).
//!
//! The human's surfaces that are not the in-proc operator — the canvas's socket
//! leg (`canvas_ws`) and the MCP leg (`mcp`, a coding CLI) — reach every agent
//! the way the shell does. The shell is the operator (ungated); a leg is not, so
//! THIS CLI — the in-proc operator — writes BOTH edges for every agent:
//!
//! - the agent's `callable_by` accepts `$subtree:<leg>` with [`VERBS`] — the leg
//!   AND everything under it;
//! - the leg's `can_call` names the agent with the same list, and so does every
//!   AI CELL under the canvas leg (the canvas's control plane, owner
//!   2026-09-30) — a heritable entry cannot serve both the leg and its
//!   descendants, so each holder carries its own (no `*` id key exists by
//!   design: each target, by id, `kernel` included).
//!
//! `*` never covers a PRIVILEGED verb, so each one is named. Done for every agent
//! when a leg is set up, and again for each agent the state stream reports
//! `added` (made by an AI, by exec, loaded from disk, or a leg itself). Every
//! grant lives in the records: the human reshapes them with `update_agent`. A
//! leg's `ingress_rule` still decides who may open frames on it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// The canvas leg's id.
pub const CANVAS_LEG: &str = crate::kern::web::CANVAS_WS_ID;

/// Every leg that gets the reach, when it exists.
pub const LEGS: [&str; 2] = [CANVAS_LEG, crate::mcp::LEG];

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

fn exists(h: &Host, id: &str) -> bool {
    h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id))
}

fn field(h: &Host, id: &str, f: &str) -> Map<String, Value> {
    h.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .and_then(|a| a.value().record().meta.get(f).and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// The legs present in this kernel.
fn legs(h: &Host) -> Vec<&'static str> {
    LEGS.into_iter().filter(|l| exists(h, l)).collect()
}

/// The HOLDERS of the reach: each present leg, and every AI agent under the
/// canvas leg (an agent that names a memory store — `ai_cells` spawns them as
/// the leg's descendants).
fn holders(h: &Host) -> Vec<String> {
    let under_canvas = |id: &str| -> bool {
        let mut cur = h.kernel.agents.get(&fantastic_kernel::AgentId::from(id)).and_then(|a| a.value().record().parent_id);
        while let Some(p) = cur {
            if p == CANVAS_LEG {
                return true;
            }
            cur = h.kernel.agents.get(&fantastic_kernel::AgentId::from(p.as_str())).and_then(|a| a.value().record().parent_id);
        }
        false
    };
    let mut v: Vec<String> = legs(h).into_iter().map(String::from).collect();
    v.extend(
        h.kernel
            .agents
            .iter()
            .filter(|e| e.value().record().meta.get("memory_store_id").and_then(Value::as_str).is_some())
            .map(|e| e.key().0.clone())
            .filter(|id| under_canvas(id)),
    );
    v
}

/// Grant every holder every verb on `ids` — both edges: each target's
/// `callable_by` accepts `$subtree:<leg>` for each present leg, and each
/// holder's `can_call` names the target (and `kernel`). One write per changed
/// record.
pub async fn grant(h: &Host, ids: &[String]) -> Result<(), String> {
    let all = json!(VERBS);
    let subs: Vec<String> = legs(h).into_iter().map(|l| format!("$subtree:{l}")).collect();
    for id in ids {
        if !exists(h, id) {
            continue;
        }
        let mut cb = field(h, id, "callable_by");
        let mut changed = false;
        for sub in &subs {
            if cb.get(sub) != Some(&all) {
                cb.insert(sub.clone(), all.clone());
                changed = true;
            }
        }
        if changed {
            let r = h.operator("kernel", json!({"type": "update_agent", "id": id, "callable_by": cb})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("reach: `{id}`.callable_by: {e}"));
            }
        }
    }
    for holder in holders(h) {
        let mut cc = field(h, &holder, "can_call");
        let mut changed = false;
        for id in ids.iter().map(String::as_str).chain(["kernel"]) {
            if id == holder || (id != "kernel" && !exists(h, id)) {
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
                return Err(format!("reach: `{holder}`.can_call: {e}"));
            }
        }
    }
    Ok(())
}

/// Grant on every agent now.
pub async fn grant_all(h: &Host) -> Result<(), String> {
    let ids: Vec<String> = h.kernel.agents.iter().map(|e| e.key().0.clone()).collect();
    grant(h, &ids).await
}

static WATCHING: AtomicBool = AtomicBool::new(false);

/// Every agent now, then each one the state stream reports `added`. The watch
/// starts once per process, whichever leg is set up first.
pub async fn start(h: &Host) -> Result<(), String> {
    grant_all(h).await?;
    if WATCHING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
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
            // a new holder (a leg, an AI cell) reaches every agent, not just the batch
            let holders_now = holders(&host);
            let r = if batch.iter().any(|b| holders_now.contains(b)) {
                grant_all(&host).await
            } else {
                grant(&host, &batch).await
            };
            if let Err(e) = r {
                tracing::warn!("{e}");
            }
        }
    });
    Ok(())
}
