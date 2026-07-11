//! Prompt assembly: render_reflect, the context how-to, and the per-turn
//! message list. NO agent menu, NO tool how-to — this backend does not run a
//! tool loop; a caller that wants tool-calling supplies its own `system_prompt`
//! teaching its convention.

use crate::history::load_history;
use crate::state::BackendState;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// Render a reflect reply into a one-line `sentence  k=v  k=v` blurb.
pub fn render_reflect(v: &Value) -> String {
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return String::new(),
    };
    let sentence = obj
        .remove("sentence")
        .and_then(|s| s.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    for (k, val) in obj.iter() {
        let rendered = match val.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(val).unwrap_or_default(),
        };
        parts.push(format!("{k}={rendered}"));
    }
    let fields = parts.join("  ");
    format!("{sentence}  {fields}").trim().to_string()
}

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances — unlike tool-calling, which is the caller's.
pub const CONTEXT_HOWTO: &str = r#"## Context window & compaction
Your live context is finite. When the conversation outgrows it, the system COMPACTS your
view: older turns are summarized or elided and a `[context-notice]` turn is inserted. The
DURABLE transcript is ALWAYS whole — compaction only shapes what you see right now, and
nothing is ever truly lost.
- To page dropped turns back on demand: `send('<your_own_id>', {type:'recall', query?, limit?})`.
  `query` substring-filters your full history; `limit` caps the page. Use it whenever you
  need a detail from earlier that isn't in your current view.
- When a compaction notice lands and the dropped span held durable facts (names, decisions,
  preferences), persist them to a memory agent (reflect the tree to find one) via the send
  tool — that is how they survive beyond this window.
- To inspect your budget + the last compaction + your own last reaction:
  `send('<your_own_id>', {type:'context_status'})`.
"#;

/// Build the per-turn message list: system block + persisted history + the user
/// turn. A caller-supplied `system_prompt` REPLACES the default system block (the
/// caller teaches its own tool convention there); the default is a lean primer +
/// self-identity + the context how-to (NO menu, NO tool how-to).
pub async fn assemble_messages(
    self_id: &AgentId,
    _state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
    system_prompt: Option<&str>,
) -> Vec<Value> {
    let system_content = match system_prompt {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => {
            // Lean primer: a flat id-index of the tree + the bundle catalog by name.
            let primer = kernel
                .send(
                    &AgentId::from("kernel"),
                    json!({"type": "reflect", "tree": "ids", "bundles": "ids"}),
                )
                .await;
            let me = kernel
                .send(self_id, json!({"type": "reflect", "tree": "none"}))
                .await;
            [
                render_reflect(&primer),
                format!("You are `{}`. {}", self_id, render_reflect(&me)),
                CONTEXT_HOWTO.to_string(),
            ]
            .join("\n\n")
        }
    };
    let mut messages = vec![json!({"role": "system", "content": system_content})];
    messages.extend(load_history(self_id, kernel, client_id).await);
    messages.push(json!({"role": "user", "content": user_text}));
    messages
}
