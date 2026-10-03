//! THE CANVAS'S ACTIVITY, host side: the `activity` agent under the canvas's
//! socket leg.
//!
//! Every OPEN question to the human — the terminal shell's `✉ Activity`, the
//! same query (`thread::waiting_for` over every agent's rows). The page must
//! never read a memory store, so this agent reads the rows AS THE OPERATOR and
//! answers with the questions only: `open` → `{open: [{agent, row, from, head,
//! ts}], to_main: [...]}`, newest first. The answer itself is an ordinary reply
//! in the asker's cell.
//!
//! `to_main` — THE MAIN CHATS' AUDIT TRAIL (owner, 2026-10-03): every message
//! that reached a canvas's MAIN chat from anyone but the human (another canvas's
//! AI, a program), the same `{agent, row, from, head, ts}`, newest first, the
//! last [`TO_MAIN`]. The main chat answers it itself; this only mirrors it. The
//! main chats are read from the page records (`main_ai_chat` on a canvas → that
//! chat cell's `backend_id`).

use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use crate::env::ClientCfg;
use crate::kern::host::Host;
use crate::store::Store;

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "activity.tools";

/// The agent's id (created by `fantastic canvas`, under the canvas's leg).
pub const ACTIVITY_ID: &str = "activity";

/// The most `to_main` entries an answer carries.
pub const TO_MAIN: usize = 50;

/// The AIs that are some canvas's main chat, from the page records: a canvas
/// naming `main_ai_chat` → that cell's `backend_id`.
pub fn main_ais(records: &[Value]) -> Vec<String> {
    let cell = |id: &str| records.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id));
    let mut out: Vec<String> = records
        .iter()
        .filter_map(|r| r.get("main_ai_chat").and_then(Value::as_str))
        .filter_map(|c| cell(c)?.get("backend_id")?.as_str().map(str::to_string))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every message in `agent`'s conversation that came from someone other than
/// the human, the canvas leg (it speaks for the human) and itself.
pub fn incoming(store: &Store, agent: &str, human: &str, leg: &str) -> Vec<crate::thread::Waiting> {
    store
        .conversation(&agent.to_string())
        .filter_map(|r| {
            let content = r.content.as_deref()?;
            let v = crate::tags::vouched(content)?;
            if v.from == agent || v.from == human || v.from == leg {
                return None;
            }
            Some(crate::thread::Waiting { row: r.id, from: v.from, head: crate::tags::head_of(content), ts: r.ts })
        })
        .collect()
}

/// The bundle.
#[derive(Default)]
pub struct ActivityBundle;

impl ActivityBundle {
    async fn open(kernel: &Arc<Kernel>) -> Value {
        let Some(root) = kernel.root().map(|r| r.id.0.clone()) else {
            return json!({"error": "activity: the kernel has no root", "reason": "not_found"});
        };
        let host = Host { kernel: Arc::clone(kernel), cfg: ClientCfg::here(), root: root.clone() };
        let agents: Vec<String> = kernel.agents.iter().map(|e| e.key().0.clone()).filter(|id| *id != root).collect();
        let mut store = Store::default();
        for a in &agents {
            let rows = host.rows(a, None).await;
            if !rows.is_empty() {
                store.apply_full(a, rows);
            }
        }
        let mut open: Vec<Value> = Vec::new();
        for a in &agents {
            // the canvas leg speaks FOR the human (a cell's turns arrive `from
            // canvas_ws`): its rows are the human's own, never questions
            for w in crate::thread::waiting_for(&store, a, crate::env::HUMAN)
                .into_iter()
                .filter(|w| w.from != crate::kern::reach::CANVAS_LEG)
            {
                open.push(json!({"agent": a, "row": w.row, "from": w.from, "head": w.head, "ts": w.ts}));
            }
        }
        open.sort_by(|x, y| y["ts"].as_f64().partial_cmp(&x["ts"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
        // the main chats' audit trail
        let records = host
            .operator(crate::kern::web::CANVAS_LOADER_ID, json!({"type": "load_tree"}))
            .await
            .result
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut to_main: Vec<Value> = Vec::new();
        for ai in main_ais(&records) {
            for w in incoming(&store, &ai, crate::env::HUMAN, crate::kern::reach::CANVAS_LEG) {
                to_main.push(json!({"agent": ai, "row": w.row, "from": w.from, "head": w.head, "ts": w.ts}));
            }
        }
        to_main.sort_by(|x, y| y["ts"].as_f64().partial_cmp(&x["ts"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
        to_main.truncate(TO_MAIN);
        json!({"open": open, "to_main": to_main})
    }
}

#[async_trait]
impl Bundle for ActivityBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    async fn handle(&self, _agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Ok(match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" => None,
            "reflect" => Some(json!({
                "handler_module": HANDLER_MODULE,
                "sentence": "Every open question to the human (the terminal shell's Activity), for the canvas: the questions only, never the rows.",
                "verbs": {"open": "No args. {open: [{agent, row, from, head, ts}], to_main: [the same]}, newest first — to_main: what reached a canvas's main chat from anyone but the human."},
            })),
            "open" => Some(Self::open(kernel).await),
            v => Some(json!({"error": format!("activity: unknown type {v:?}"), "reason": "unhandled"})),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_main_ais_are_read_from_the_canvases_main_chat_cells() {
        let recs = vec![
            json!({"id": "canvas", "main_ai_chat": "v1"}),
            json!({"id": "v1", "backend_id": "ai_1"}),
            json!({"id": "sql", "main_ai_chat": "v2"}),
            json!({"id": "v2", "backend_id": "ai_2"}),
            json!({"id": "v3", "backend_id": "ai_3"}),
            json!({"id": "gone", "main_ai_chat": "nope"}),
        ];
        assert_eq!(main_ais(&recs), vec!["ai_1".to_string(), "ai_2".to_string()]);
    }

    #[test]
    fn a_main_chats_incoming_is_what_others_sent_it_never_the_human_or_itself() {
        let mut s = Store::default();
        s.apply_full(
            &"ai_1".to_string(),
            [
                json!({"id": 1, "agent": "ai_1", "ts": 1.0, "content": "[from user · msg 1]\nhello"}),
                json!({"id": 2, "agent": "ai_1", "ts": 2.0, "content": "[from canvas_ws · msg 2]\nfrom a cell"}),
                json!({"id": 3, "agent": "ai_1", "ts": 3.0, "content": "[from ai_sql · msg 3]\nthe report is ready"}),
                json!({"id": 4, "agent": "ai_1", "ts": 4.0, "content": "[from ai_1 · msg 4]\nnote to self"}),
            ]
            .into_iter()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect(),
        );
        let got = incoming(&s, "ai_1", "user", "canvas_ws");
        let heads: Vec<(&str, &str)> = got.iter().map(|w| (w.from.as_str(), w.head.as_str())).collect();
        assert_eq!(heads, vec![("ai_sql", "the report is ready")]);
    }
}
