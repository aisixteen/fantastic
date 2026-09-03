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
use crate::history_view::{dedupe_stale_reads, drop_answered_reflects};
use crate::state::BackendState;
use fantastic_kernel::{AgentId, Item, Kernel};
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
asked. Interpret instructions literally — act on what is asked, not on inferred
intent. Keep prose brief: the work happens in calls, not in explanation.

## Sending verbatim content — `send_binary`
When a payload field's VALUE is a body of text you are authoring (source, a document,
a patch), a JSON string breaks on escaping — use `send_binary` with the FENCED form:
keep the JSON small and complete, put `"@body"` where that value goes, and write the
body OUTSIDE the JSON between two marker lines, still inside the same tool_call:
<tool_call>{"name":"send_binary","arguments":{"target_id":"<id>","payload":{…,"bytes":"@body"}}}
*** Begin Bytes
<the body, verbatim — real newlines, real quotes, nothing escaped>
*** End Bytes
</tool_call>
Both markers start at column 0. Everything between them rides VERBATIM — no escaping,
any content, any size; nothing to quote, balance or count. The JSON line above them
is ordinary and small (`target_id`, `reply_to`, `id`, `timeout`, `retries` live
there). Which verbs take a body, and in which field, is each agent's own story —
reflect the agent to learn its composition; `into` names a field other than `bytes`.

A `<tool_call>` IS AN INVOCATION, WHEREVER YOU WRITE IT — including inside your own
thinking. The envelope is parsed out of everything you emit and dispatched the
moment it is complete, so there is no hypothetical or illustrative call: writing one
makes it happen.

Your thinking is NOT carried over between turns — it is stripped from history, so a
conclusion you only reasoned about is lost. Put what you worked out into the call you
make, or into a message. Reasoning longer never carries further than acting now does.
Before ending a turn, check one thing: every action you decided on exists as a
`<tool_call>` envelope in the turn. A decision without its envelope did not happen.

Stopping is EXPLICIT and costs one line: when the work is done, say so and emit
<tool_call>{"name":"done"}</tool_call>
— that ends your chain. A turn with NO call ends nothing: the runtime reminds you
once (a call decided in thinking does not execute unless the envelope is written
out), so never leave a finished task unsignalled — call `done`.

NEEDING A HUMAN IS ALSO NORMAL. When you cannot proceed without an answer, ask and
stop: put the question in your message — STRICT JSON on its own line, so a UI
can parse and render it:
`{"type":"question", "request_id":"<yours>", "kind":"text|single|multi|confirm",
"question":"...", "options":[...]}` — then emit no call. The chain
ends; the ANSWER arrives as your next turn (name your request_id to correlate),
with your full history intact — continue exactly where you stopped. Nothing you
completed before asking will re-run.

HOW REPLIES ARRIVE. Every send is dispatched under a PID (`<agent>:<token>`) and
every reply comes back tagged `[reply to <pid> · <agent> <verb>]`. Read that
literally:

- ALL the calls you make in ONE turn are answered TOGETHER in your next turn —
  one block per call, each tagged `· N of M in batch <token>` when you made
  several. M is complete on arrival: there is nothing still coming from that
  batch, so never wait, poll, or re-ask for a "missing" reply — if a call
  failed or timed out, its block says so.
- The calls of one turn run CONCURRENTLY. Their side effects have NO defined
  order — if B must happen after A, put B in the next turn, after A's reply.
- Each call may carry `timeout: <seconds>` in its arguments to bound that call;
  otherwise a generous default applies. A call that exceeds it is CANCELLED and
  its block reports the timeout.
- A call may carry `id: "<name>"` in its arguments — BESIDE `payload`, never
  inside it: `{"name":"send","arguments":{"target_id":"x","payload":{...},
  "id":"job-1"}}`. That NAMES the execution.
  Re-sending the SAME id NEVER runs the work twice: you get the first run's
  recorded outcome back, byte for byte (even if it failed). Use it for any call
  you must not double-fire (a write, a payment, a create) and to safely re-fetch
  a reply; a genuinely NEW attempt needs a NEW id. Ids may not contain ':'.
  Add `retries: N` and the KERNEL re-runs a failing or timing-out call up to N
  more times before answering — you send once and receive the final outcome
  (`attempts` says how many runs it took). Work that stays failed is parked:
  `send 'kernel' {type:'ledger'}` lists your executions and what became of
  them, outcomes included — inspect there instead of re-firing blind.
- `reply_to:null` means the call still runs but its answer is discarded — it is
  not in M and nothing comes back. `reply_to:<someone else>` hands that block to
  THEM instead of you.
- A reply whose PID you do not recognise is NORMAL — another agent can route its
  result to you. Read it, use it if useful, and move on.

A `reflect` reply is LARGE and TRANSIENT — you see its full text once, in the turn it
arrives, and the reply itself carries the instruction for what to record before it
goes. Follow it there. If you later need a detail you did not keep, reflect that agent
again — it is cheap and always current, and re-reading beats guessing.
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

/// Build the per-turn message list: system block + the MODEL VIEW of the
/// persisted history + the user turn. A caller-supplied `system_prompt` REPLACES
/// the default system block (a caller that wants a different role, or a different
/// convention, owns it whole); the default is a lean primer + self-identity +
/// [`SEND_TO_HOWTO`] + [`CONTEXT_HOWTO`], and NO agent menu.
///
/// The system block is rebuilt on EVERY turn and is never persisted, so it always
/// reflects the tree as it is at send-time; the durable store holds only the
/// user/assistant turns.
///
/// Returns `(messages, store_len)`. `store_len` is the DURABLE store's length
/// behind those messages, and it is NOT derivable from them: the history is a
/// deduped/trimmed VIEW (see [`crate::history_view`]), so counting the view puts
/// the compaction reaction cursor short by exactly the turns the trimmers dropped
/// — and [`crate::projection::derive_reaction`] then scans turns written BEFORE
/// the notice and scores a reaction that never happened.
pub async fn assemble_messages(
    self_id: &AgentId,
    _state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
    system_prompt: Option<&str>,
) -> (Vec<Value>, usize) {
    let system_content = match system_prompt {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => {
            // Lean primer: a flat id-index of the tree + the bundle catalog by name.
            let primer = kernel
                .send(vec![Item::new(
                    AgentId::from("kernel"),
                    json!({"type": "reflect", "tree": "ids", "bundles": "ids"}),
                )])
                .await
                .remove(0)
                .result;
            let me = kernel
                .send(vec![Item::new(
                    self_id.clone(),
                    json!({"type": "reflect", "tree": "none"}),
                )])
                .await
                .remove(0)
                .result;
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
    // A trimmed VIEW, not a rewrite of the store: stale whole-state reads deduped
    // and answered reflects reduced to a stub — the model's own distillation
    // carries the facts forward. Durable history keeps every turn, so the trace
    // stays complete and `recall` pages anything back.
    let store = load_history(self_id, kernel, client_id).await;
    let store_len = store.len();
    messages.extend(drop_answered_reflects(&dedupe_stale_reads(&store)));
    messages.push(json!({"role": "user", "content": user_text}));
    (messages, store_len)
}
