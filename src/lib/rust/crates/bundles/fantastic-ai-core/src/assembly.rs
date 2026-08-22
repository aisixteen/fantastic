//! Prompt assembly: render_reflect, the two how-tos, and the per-turn message
//! list.
//!
//! The default system block states the MECHANISM and NO strategy. NO agent menu
//! (the primer's id-index is the tree, and the model reflects what it needs);
//! nothing about looping, orienting, iterating or finishing. A caller that wants
//! a different role supplies its own `system_prompt`, which REPLACES this block
//! whole.
//!
//! That line is load-bearing and it is TESTED (`the_default_prompt_states_the_
//! mechanism_not_a_strategy`, mirroring python). The removed strategy prompt said
//! "either call again or write your final answer to finish", "ORIENT FIRST",
//! "NEVER claim you don't have access" — that is the loop, dictated. Reusing that
//! wording because it is "tuned" would quietly void every emergence result taken
//! afterwards.

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

/// How to reach another agent — the ENVELOPE this crate actually dispatches (see
/// [`crate::route::route_reply`]).
///
/// Lands in the default system block only because routing exists: a prompt that
/// teaches an envelope nothing dispatches is strictly worse than no prompt at all,
/// so this text and the router are one change.
///
/// States facts, never a strategy: how to address an agent, what `reflect`
/// returns, where `reply_to` sends the answer, how to recognise a reply. It does
/// not say to loop, to keep going, or how to decide it is finished — stopping is
/// emitting no call, which costs nothing and needs no instruction.
pub const SEND_TO_HOWTO: &str = r#"## Reaching other agents
Everything is an agent with a string id. `send(target_id, payload)` reaches any of them —
the payload's `type` names the verb; `{type:'reflect'}` returns that agent's verbs and
state. To send, emit exactly this, alone on its line:
<tool_call>{"name":"send","arguments":{"target_id":"<id>","payload":{"type":"<verb>"},"reply_to":"self"}}</tool_call>
`reply_to` is where that send's reply goes. It defaults to you — leave it out and the
answer comes back as your next message. Name another agent's id to hand the answer to
them instead, or `null` to discard it. Text outside the tag is your message to whoever
asked. A message tagged `[reply to <pid> · <id> <verb>]` is the reply to the send you issued
naming that agent and that verb — replies can lag a turn behind when several calls are
in flight, so match the tag before assuming a call was lost and re-issuing it.
"#;

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances alongside [`SEND_TO_HOWTO`]'s addressing.
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
/// turn. A caller-supplied `system_prompt` REPLACES the default system block (a
/// caller that wants a different role, or a different convention, owns it whole);
/// the default is a lean primer + self-identity + [`SEND_TO_HOWTO`] +
/// [`CONTEXT_HOWTO`], and NO agent menu.
///
/// The system block is rebuilt on EVERY turn and is never persisted, so it always
/// reflects the tree as it is at send-time; the durable store holds only the
/// user/assistant turns.
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
                SEND_TO_HOWTO.to_string(),
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
