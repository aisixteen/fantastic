//! THE CANVAS'S ACTIVITY, host side: the `activity` agent under the canvas's
//! socket leg.
//!
//! Every OPEN question to the human — the terminal shell's `✉ Activity`, the
//! same query (`thread::waiting_for` over every agent's rows). The page must
//! never read a memory store, so this agent reads the rows AS THE OPERATOR and
//! answers with the questions only: `open` → `{open: [{agent, row, from, head,
//! ts}]}`, newest first. The answer itself is an ordinary reply in the asker's
//! cell.

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
                .filter(|w| w.from != crate::kern::canvas_reach::LEG)
            {
                open.push(json!({"agent": a, "row": w.row, "from": w.from, "head": w.head, "ts": w.ts}));
            }
        }
        open.sort_by(|x, y| y["ts"].as_f64().partial_cmp(&x["ts"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
        json!({"open": open})
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
                "verbs": {"open": "No args. {open: [{agent, row, from, head, ts}]}, newest first."},
            })),
            "open" => Some(Self::open(kernel).await),
            v => Some(json!({"error": format!("activity: unknown type {v:?}"), "reason": "unhandled"})),
        })
    }
}
