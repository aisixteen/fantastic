//! Reply routing — where ONE provider pass's output goes. Mirrors python
//! `ai_core/core.py:_route_reply`, which is canonical.
//!
//! After one provider pass, its output goes where it was ADDRESSED. Two paths,
//! and the first is the model's:
//!
//!   the model's own `<call>`s — each names a target, a payload and
//!     `reply_to`. All are dispatched, and each reply is delivered to its own
//!     `reply_to`. `reply_to:'self'` is what makes a chain, and the chain
//!     continues ONLY because the model asked again. No call ends the turn: stop
//!     is the default.
//!
//!   the caller's `send_to` — hands this pass's TEXT to another agent, once,
//!     without propagating.
//!
//! Nothing here bounds how far the model goes. Every dispatch is a `ps` row and
//! `cancel` kills a subtree, so a runaway is visible and killable exactly like a
//! script agent's `while(true)`. Deciding how much thinking is allowed is not
//! this module's call — and NO per-route timeout is added on purpose: N calls in
//! one pass serialize into N turns inside one route task, and python deliberately
//! bounds nothing there either.
//!
//! DETACHED is the one hard requirement. `BackendState::lock` is a non-reentrant
//! `tokio::sync::Mutex`, so awaiting a self-addressed send from inside the holder
//! would stall until the per-send ceiling (180s). Spawned as its own kernel task
//! (see [`crate::verbs::send`]), the follow-up queues behind the current turn.
//!
//! The `_call_stack` recursion guard is NOT propagated from here: python's
//! `_deliver` does not carry it either, and a `reply_to:self` chain that did
//! would trip [`fantastic_kernel::call_guard`] as a false cycle on turn 2. The
//! INBOUND check lives in [`crate::verbs::send`], before the lock.

use crate::constants::MAX_SILENT_TURNS;
use crate::events::emit_status;
use crate::helpers::mint_batch_token;
use crate::history_view::{docs_head, example_verb, point_to_docs};
use crate::state::state_for;
use crate::tool_parse::{malformed_calls, terminated_calls};
use fantastic_bundle::{py_repr, py_repr_value, py_str, py_strip, py_truthy};
use fantastic_kernel::outcome::Status;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

/// ROUTING TURNS with a live batch, per agent — the first summand of `await`'s
/// `pending`. It counts TURNS, not calls: what the model is waiting for is a
/// batch's answer, and a batch is answered whole.
pub(crate) fn inflight() -> &'static Mutex<HashMap<String, u64>> {
    static INFLIGHT: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn inflight_bump(self_id: &str, by: i64) {
    let mut m = inflight().lock().expect("inflight poisoned");
    let e = m.entry(self_id.to_string()).or_insert(0);
    *e = e.saturating_add_signed(by);
}

fn inflight_count(self_id: &str) -> u64 {
    *inflight()
        .lock()
        .expect("inflight poisoned")
        .get(self_id)
        .unwrap_or(&0)
}

/// What `await` is told when there is nothing to wait for. DELIVERED TO SELF, as
/// a fresh turn: it must WAKE the model, not sit in a return value nobody reads.
const AWAIT_REFUSED: &str = "[await refused] nothing of yours is in flight and \
nothing is queued for you, so there is nothing to wait for. Act — make the call — \
or end the chain with <call name=\"done\"></call>.";

/// What a call that named a VERB but no agent gets told.
///
/// A PROMPT, not a diagnostic — it is read by a model, so it names the SHAPE and
/// not just the fault, and it is byte-identical across runtimes (pinned by a
/// literal-text test). [`crate::tool_parse`] already translates a verb-named tool
/// into a send whenever the args say WHERE to send it; what reaches here named a
/// verb and no agent, and that is a gap only the model can close — guessing a
/// target would dispatch a write to whichever agent we picked.
///
/// Measured 2026-08-06: gemma-4-12B emitted
/// `{"name":"reflect","arguments":{"detail":"full"}}` and the bare fault told it
/// nothing about how to retry, so it kept re-deriving the envelope from the
/// system prompt instead. Every other error on this path states the fix; this one
/// used not to.
pub const NO_TARGET_ERROR: &str = "no target_id, so nothing was dispatched — a \
send must name the AGENT it goes to. Shape: <call name=\"send\"><target_id>agent\
</target_id><type>verb</type></call>. Send `kernel` <type>reflect</type> for the \
list of agent ids.";

/// What a send with no payload gets told, `{target}` filled in.
///
/// NO PAYLOAD, NO SEND. Dispatching nothing reaches the handler as nothing and
/// raises about OUR internals, naming neither the verb the model omitted nor the
/// agent it addressed (python: `'NoneType' object has no attribute 'get'`,
/// measured three times in one run on 2026-08-06). [`crate::tool_parse`]
/// translates every compressed shape it can recognise; what arrives here with no
/// payload really has no verb in it, and saying so is the useful answer. Same
/// shape as [`NO_TARGET_ERROR`], byte-identical across runtimes.
pub fn no_payload_error(target: &str) -> String {
    // `{target!r}`: python's repr picks the quote (double when the text holds a
    // single one) and escapes `\`, `\t`, `\n` — wrapping the raw text in single
    // quotes printed `send to 'it's'` and `send to 'tab	there'`.
    format!(
        "send to {} carried no payload, so nothing was dispatched. \
         A call names its verb in `<type>`, e.g. <type>reflect</type>.",
        py_repr(target)
    )
}

/// Read the target's incarnation and hand `text` over — python's
/// `_deliver_soon`, whose whole job is to capture `born` BEFORE the delivery
/// runs. A delivery that crosses the target's next boot is recorded, not acted
/// on (see [`deliver`]).
pub(crate) fn deliver_soon(
    kernel: &Arc<Kernel>,
    target: &str,
    text: &str,
    client_id: &str,
    system_prompt: Option<&str>,
    notice: bool,
) {
    let born = crate::state::incarnation(&AgentId::from(target));
    deliver_soon_with_born(kernel, target, text, client_id, system_prompt, born, notice);
}

/// NOT AWAITED. A `send` to an AI agent takes that agent's FIFO lock and does
/// not return until the whole turn is done — model call, streaming, and that
/// turn's own routing. Awaiting each delivery inline made reply 2 of a 4-call
/// turn wait for the entire conversation reply 1 started, and if the chain ended
/// first it was never delivered at all: measured on python 2026-08-24, 26 of 36
/// calls never answered, misread three separate times as a model being stubborn.
///
/// A KERNEL TASK named `deliver_reply` on the TARGET (python
/// `kernel.ctx.spawn_task(target, "deliver_reply", coro)`), not a bare spawn: it
/// is a `ps` row, `cancel` reaches it, and it cannot be orphaned.
#[allow(clippy::too_many_arguments)]
fn deliver_soon_with_born(
    kernel: &Arc<Kernel>,
    target: &str,
    text: &str,
    client_id: &str,
    system_prompt: Option<&str>,
    born: u64,
    notice: bool,
) {
    let target_id = AgentId::from(target);
    let work_kernel = Arc::clone(kernel);
    let (target, text) = (target.to_string(), text.to_string());
    let client_id = client_id.to_string();
    let system_prompt = system_prompt.map(str::to_string);
    kernel.spawn_task(&target_id, "deliver_reply", async move {
        deliver(
            &work_kernel,
            &target,
            &text,
            &client_id,
            system_prompt.as_deref(),
            born,
            notice,
        )
        .await;
    });
}

/// Hand `text` to `target` as a fresh turn. Exactly python's `_deliver`.
///
/// `pub(crate)` only so the late-reply twin can hand it a STALE `born`, exactly
/// as python's own test calls `_deliver(..., born=1)`.
///
/// `born` is the target's incarnation as of when this delivery was DECIDED. A
/// boot bumps that counter, so a reply that crosses a reboot answers a call the
/// previous incarnation made: the agent that asked is gone and the one that
/// replaced it is on other work. It is kept as a ROW — real data, written by the
/// TARGET into its own store (the send is marked `late`) — but it wakes no turn.
/// Measured 2026-09-06 (python): the rebooted agent was woken by answers to its
/// predecessor's calls and finished THAT work instead of the new instruction.
pub(crate) async fn deliver(
    kernel: &Arc<Kernel>,
    target: &str,
    text: &str,
    client_id: &str,
    system_prompt: Option<&str>,
    born: u64,
    notice: bool,
) {
    let target_agent = AgentId::from(target);
    let late = crate::state::incarnation(&target_agent) != born;
    let mut payload = Map::new();
    payload.insert("type".to_string(), json!("send"));
    payload.insert("text".to_string(), json!(text));
    payload.insert("client_id".to_string(), json!(client_id));
    // HARNESS TEXT, NOT A MESSAGE (owner, 2026-10-07): a turn this layer writes to
    // its own agent is stored as `kind: "notice"`, so a view can hide it; the model
    // still reads it as a user turn. Honoured only on a send to self. Python
    // `_deliver`.
    if notice {
        payload.insert("notice".to_string(), json!(true));
    }
    if late {
        // A LATE REPLY TO A PREVIOUS INCARNATION. Kept as a ROW — it is real data —
        // but it does not wake a turn: the agent that made the call is gone, and
        // the one that replaced it is on other work. The TARGET writes it (a store
        // keys every row by its writer), so it rides a `send` marked `late`.
        payload.insert("late".to_string(), json!(true));
    }
    if let Some(sp) = system_prompt {
        payload.insert("system_prompt".to_string(), json!(sp));
    }
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(target),
            Value::Object(payload),
        )])
        .await
        .remove(0);

    // ANNOUNCE A FAILED HANDOFF. If the destination could not take the turn —
    // it is gated, gone, or its own send answered `{"error": …}` — say so on its
    // inbox instead of dropping the reply.
    //
    // Discarding it made a chain that DIED mid-flight identical to a model that
    // chose to stop: no error anywhere, the last turn simply never happened.
    // Only the first send's error was ever visible, i.e. never the ones that
    // matter. This does NOT raise: the routing dispatch is detached, and
    // unwinding here would kill sibling branches that are still fine.
    //
    // THE ANNOUNCEMENT IS AN ORDINARY `status` EVENT — same shape a failed direct
    // send produces (`{type:'status', source, phase:'done', detail:{…}, ts,
    // client_id}`), because that is the shape reflect's `emits` documents and the
    // shape a UI/harness subscribes to. It rode with `reason`/`error` at the TOP
    // level here until 2026-09-02, so every subscriber reading `ev.detail.reason`
    // saw nothing — the silent-death regression this announcement exists for,
    // wearing the announcement's own clothes.
    // FAILED IS `failed`/`timed_out` — python `out["status"] in ("failed",
    // "timed_out")` — plus any truthy `error` on the reply itself. `cancelled`
    // is NOT announced: an operator kill already told whoever asked for it, and
    // rust's `!is_completed()` turned every cancel into a spurious error event.
    let reply_error = out.result.get("error").filter(|v| py_truthy(v));
    let failed = matches!(out.status, Status::Failed | Status::TimedOut) || reply_error.is_some();
    if failed {
        // The reply's own error, else the RAW status — python passes
        // `out["status"]`, so the text is `failed`/`timed_out` with no prefix.
        // Rust wrote `delivery failed`, a string no python subscriber matches.
        let why = reply_error.map_or_else(|| json!(out.status.as_str()), |e| e.clone());
        let target_id = AgentId::from(target);
        let mut detail = Map::new();
        detail.insert("reason".to_string(), json!("error"));
        detail.insert("error".to_string(), why);
        emit_status(
            kernel,
            &state_for(&target_id),
            &target_id,
            client_id,
            "done",
            detail,
        )
        .await;
    }
}

/// The reply a model reads: the outcome's result with its raw reply body — any
/// verb may answer one — decoded utf-8 (replace) AT ITS SLOT, python's
/// `render_reply` (`bytes are decoded utf-8 (replace)`) over a reply dict that
/// holds the bytes there. A `read_stream` chunk reads as its text in `bytes`.
fn reply_as_text(out: &fantastic_kernel::Outcome) -> std::borrow::Cow<'_, Value> {
    let Some(b) = &out.body else {
        return std::borrow::Cow::Borrowed(&out.result);
    };
    let mut shown = out.result.clone();
    let text = Value::String(String::from_utf8_lossy(&b.bytes).into_owned());
    // a slot the result cannot hold (a non-object reply) leaves it as it is
    let _ = b.place(&mut shown, text);
    std::borrow::Cow::Owned(shown)
}

/// The args echoed back with an error, minus `payload` — python's
/// `{k: v for k, v in args.items() if k != "payload"}`. Shows the model the
/// ROUTING it wrote, which is the part that was wrong, without re-quoting a
/// program body at it.
/// A title lives INSIDE the reply tag, so it may not carry the one character
/// that closes the tag, nor a newline that would end the tag line. Capped at 140
/// characters: it is a sentence, not a body.
fn clean_title(raw: Option<&Value>) -> Option<String> {
    let text = raw?.as_str()?;
    let mut out = String::new();
    let mut gap = false;
    for c in text.chars() {
        if c == ']' || c == '\r' || c == '\n' {
            gap = true;
        } else {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        }
    }
    // PYTHON'S STRIP (White_Space + `\x1c`–`\x1f`) and 140 CODE POINTS:
    // `trim()` left an information separator on the title.
    let out: String = py_strip(&out).chars().take(140).collect();
    (!out.is_empty()).then_some(out)
}

/// Expand every `<to>` list into one call per recipient, each carrying the whole
/// list in its payload. A call with no `<to>` passes through untouched.
pub(crate) fn fan_out(
    calls: Vec<crate::tool_parse::ToolCall>,
    message_id: Option<i64>,
) -> Vec<crate::tool_parse::ToolCall> {
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        let recipients: Vec<String> = match call.arguments.get("to") {
            Some(Value::Array(items)) if !items.is_empty() => items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            _ => Vec::new(),
        };
        if call.name != "send" || recipients.is_empty() {
            // A PLAIN SEND TO A PEER AI carries `msg` too — the sending turn's
            // id — so the peer's `[from … · msg N]` header names something the
            // sender can be asked about (python: `payload.setdefault("msg")`).
            let mut call = call;
            let is_peer_send = call
                .arguments
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                == Some("send");
            if call.name == "send" && is_peer_send {
                let re = call.arguments.shift_remove("re");
                let stream_id = call.stream_id.clone();
                if let Some(Value::Object(p)) = call.arguments.get_mut("payload") {
                    if let Some(t) = message_id {
                        p.entry("msg".to_string()).or_insert(json!(t));
                    }
                    // A MESSAGE carries the stream it was previewed as (only a
                    // message: a verb call's payload stays exactly what the
                    // model wrote).
                    if let Some(sid) = stream_id.filter(|s| !s.is_empty()) {
                        p.insert("stream_id".to_string(), json!(sid));
                    }
                    if let Some(r) = re {
                        p.insert("re".to_string(), r);
                    }
                }
            }
            out.push(call);
            continue;
        }
        let re = call.arguments.get("re").cloned();
        let base_payload = match call.arguments.get("payload") {
            Some(Value::Object(p)) => p.clone(),
            _ => Map::new(),
        };
        for r in &recipients {
            let mut payload = base_payload.clone();
            // The WHOLE list, in every copy — that is what makes it one message.
            payload.insert(
                "to".to_string(),
                Value::Array(recipients.iter().map(|x| json!(x)).collect()),
            );
            if let Some(t) = message_id {
                payload.insert("msg".to_string(), json!(t));
            }
            if let Some(sid) = call.stream_id.as_deref().filter(|s| !s.is_empty()) {
                if payload.get("type").and_then(Value::as_str) == Some("send") {
                    payload.insert("stream_id".to_string(), json!(sid));
                }
            }
            if let Some(v) = &re {
                payload.insert("re".to_string(), v.clone());
            }
            let mut args = Map::new();
            for (k, v) in &call.arguments {
                // `to` became the fan-out, `re` moved into the payload, and
                // `target_id` is the recipient this copy goes to.
                if k != "to" && k != "re" && k != "target_id" && k != "payload" {
                    args.insert(k.clone(), v.clone());
                }
            }
            args.insert("target_id".to_string(), json!(r));
            args.insert("payload".to_string(), Value::Object(payload));
            out.push(crate::tool_parse::ToolCall {
                id: call.id.clone(),
                name: "send".to_string(),
                arguments: args,
                stream_id: call.stream_id.clone(),
            });
        }
    }
    out
}

/// How to get a reply's body again, in ONE line: the replying agent's own
/// `origin` when it wrote one (a file_bridge read names path, range and version);
/// otherwise the call that produced it — re-sending it may not give the same
/// bytes, and the line says so. Python `_origin_line`.
#[must_use]
pub fn origin_line(result: &Value, target: &str, verb: &str, payload: &Value) -> String {
    if let Some(o) = result.get("origin").and_then(Value::as_str) {
        if !py_strip(o).is_empty() {
            return fantastic_bundle::py_split_whitespace(o)
                .join(" ")
                .chars()
                .take(300)
                .collect();
        }
    }
    let mut args = Map::new();
    if let Some(p) = payload.as_object() {
        for (k, v) in p {
            if !matches!(k.as_str(), "type" | "client_id" | "_call_stack") {
                args.insert(k.clone(), v.clone());
            }
        }
    }
    let shown: String = fantastic_bundle::py_json_dumps(&Value::Object(args), false, None)
        .chars()
        .take(200)
        .collect();
    format!("{target} {verb} {shown} — re-send it; the output may differ")
}

fn you_sent(args: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for (k, v) in args {
        if k != "payload" {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Honour where this pass's output goes.
///
/// Runs DETACHED, after the FIFO guard is dropped — see the module header. Takes
/// owned values because it is spawned as its own kernel task and must be
/// `'static`.
///
/// ONE TURN, ONE BATCH, ONE MESSAGE PER DESTINATION: every call the model
/// emitted is dispatched together, and everything owed to a destination —
/// results, parse errors, shape errors — arrives as ONE delivered turn.
/// THE SILENT STREAK per chain `(agent id, client id)`: turns in a row that
/// dispatched nothing. Any call — `done` and `await` included — ends it; at
/// [`MAX_SILENT_TURNS`] the chain gives up. Python `_silent`.
pub(crate) fn silent() -> &'static Mutex<HashMap<(String, String), usize>> {
    static SILENT: OnceLock<Mutex<HashMap<(String, String), usize>>> = OnceLock::new();
    SILENT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An `inquiry` an AI received and has not answered yet — python's
/// `_open_inquiries` value: the asker (`None` for the operator), the pid its reply
/// tag carries, the asker's own thread, and the request id.
pub(crate) struct OpenInquiry {
    pub(crate) asker: Option<String>,
    pub(crate) pid: String,
    pub(crate) reply_client: String,
    pub(crate) request_id: Value,
}

/// OPEN INQUIRIES (owner, 2026-10-06: fully async), by the receiver's chain
/// `(receiver, client_id)`. The answer the model gives in ANY turn of that chain —
/// or the news that it ended without one — goes to the asker as a later message
/// under the inquiry's pid. Python `_open_inquiries`.
pub(crate) fn open_inquiries() -> &'static Mutex<HashMap<(String, String), OpenInquiry>> {
    static OPEN: OnceLock<Mutex<HashMap<(String, String), OpenInquiry>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Answers an agent is still OWED, by inquiry pid: added when its inquiry comes
/// back `accepted`, cleared when a message under that pid arrives. They count as
/// work in flight, so `await` is honest while one is out. Python `_awaiting`.
pub(crate) fn awaiting() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static AWAITING: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    AWAITING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Settle what `self_id` was owed: every `[reply to <pid> · <who> inquiry]` tag in
/// an incoming message clears that pid. Python `_send`'s `_ANSWER_TAG` scan.
pub(crate) fn settle_owed(self_id: &str, text: &str) {
    let mut owed = awaiting().lock().expect("awaiting poisoned");
    let Some(set) = owed.get_mut(self_id) else {
        return;
    };
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("[reply to ") else {
            continue;
        };
        let Some((pid, tail)) = rest.split_once(" · ") else {
            continue;
        };
        if tail
            .split(']')
            .next()
            .is_some_and(|t| t.ends_with(" inquiry"))
        {
            set.remove(pid);
        }
    }
}

/// Close the inquiry this chain `(self_id, client_id)` is answering: hand `body`
/// (`{answer}` or `{error}`) to its asker as `[reply to <pid> · <self_id> inquiry]`.
/// The answer is THIS agent's own send to the asker (owner, 2026-10-06), gated like
/// any; a refused delivery is told to this agent. An operator's inquiry has no
/// conversation to land in: the answer is an event on this agent's inbox. No-op
/// when nothing is open. Python `_close_inquiry`.
pub(crate) async fn close_inquiry(
    kernel: &Arc<Kernel>,
    self_id: &str,
    client_id: &str,
    mut body: Map<String, Value>,
) {
    let open = open_inquiries()
        .lock()
        .expect("open inquiries poisoned")
        .remove(&(self_id.to_string(), client_id.to_string()));
    let Some(open) = open else {
        return;
    };
    body.insert("request_id".to_string(), open.request_id.clone());
    let Some(asker) = open.asker else {
        let mut ev = Map::new();
        ev.insert("type".to_string(), json!("inquiry_answer"));
        ev.insert("re".to_string(), json!(open.pid));
        ev.extend(body);
        kernel
            .emit(&AgentId::from(self_id), Value::Object(ev))
            .await;
        return;
    };
    let shown: Map<String, Value> = body.into_iter().filter(|(_, v)| !v.is_null()).collect();
    let text = format!(
        "[reply to {} · {self_id} inquiry]\n{}",
        open.pid,
        crate::tool_parse::render_reply(&Value::Object(shown))
    );
    let (k, me, chain, reply_client) = (
        Arc::clone(kernel),
        self_id.to_string(),
        client_id.to_string(),
        open.reply_client,
    );
    kernel.spawn_task(&AgentId::from(self_id), "inquiry_answer", async move {
        deliver_answer(&k, &me, &chain, &asker, &reply_client, &text).await;
    });
}

/// The answer, sent AS THIS AGENT into the asker's conversation — a send to an AI
/// runs the asker's whole turn, so this runs as its own task, never inline. A
/// REFUSED answer is not dropped: this agent is told why, in its own chain. Python
/// `_deliver_answer`.
async fn deliver_answer(
    kernel: &Arc<Kernel>,
    self_id: &str,
    client_id: &str,
    asker: &str,
    reply_client: &str,
    text: &str,
) {
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(asker),
            json!({"type": "send", "text": text, "client_id": reply_client}),
        )])
        .await
        .remove(0);
    let err = out.result.get("error").filter(|v| py_truthy(v));
    if err.is_some() || matches!(out.status, Status::Failed | Status::TimedOut) {
        let why = err.map_or_else(|| out.status.as_str().to_string(), py_str);
        deliver_soon(
            kernel,
            self_id,
            &format!("[your inquiry answer did not reach {asker}: {why}]"),
            client_id,
            None,
            true,
        );
    }
}

/// The model's `inquiry_answer`: delivered to the asker of the inquiry this chain
/// is answering. `Some` is the error block for the model — nothing to answer, or
/// the placeholder. Python `_answer_inquiry`.
pub(crate) async fn answer_inquiry(
    kernel: &Arc<Kernel>,
    self_id: &str,
    client_id: &str,
    call: &crate::tool_parse::ToolCall,
) -> Option<String> {
    let open = open_inquiries()
        .lock()
        .expect("open inquiries poisoned")
        .contains_key(&(self_id.to_string(), client_id.to_string()));
    if !open {
        return Some(
            "inquiry_answer: nothing asked you in this run — an answer goes to an inquiry you \
             received (`[inquiry from …]`)"
                .to_string(),
        );
    }
    let answer = call.arguments.get("answer").cloned().unwrap_or(Value::Null);
    // THE PLACEHOLDER IS NOT AN ANSWER — a model copies the shown example back
    // verbatim (measured 2026-09-04, peer_review: "..." written as the verdict).
    let real = answer
        .as_str()
        .filter(|s| !py_strip(s).trim_matches('.').is_empty());
    let Some(real) = real else {
        return Some(format!(
            "that is the PLACEHOLDER, not an answer — `answer` came back as {}. Say what \
             you actually concluded.",
            py_repr_value(&answer)
        ));
    };
    let mut body = Map::new();
    body.insert("answer".to_string(), json!(real));
    close_inquiry(kernel, self_id, client_id, body).await;
    None
}

/// Drop a deleted agent's routing count and every silent streak of its chains
/// ([`crate::verbs::on_delete`]).
pub(crate) fn forget(self_id: &str) {
    inflight()
        .lock()
        .expect("inflight poisoned")
        .remove(self_id);
    silent()
        .lock()
        .expect("silent poisoned")
        .retain(|chain, _| chain.0 != self_id);
    open_inquiries()
        .lock()
        .expect("open inquiries poisoned")
        .retain(|chain, _| chain.0 != self_id);
    awaiting()
        .lock()
        .expect("awaiting poisoned")
        .remove(self_id);
}

/// What a caller is told when a chain gives up ([`MAX_SILENT_TURNS`] silent
/// turns). Python `_GAVE_UP`, byte for byte.
pub fn gave_up_text() -> String {
    format!(
        "The LLM was not able to answer — {MAX_SILENT_TURNS} turns in a row produced \
         no call. Reiterate the inquiry."
    )
}

/// Prefixed to the reminder when the turn it answers was DEAD (only thinking).
/// Python `_DEAD_TURN_LINE`.
pub const DEAD_TURN_LINE: &str =
    "[no answer] Your last turn produced only thinking and no answer — it was discarded. ";

/// THE NO-CALL NUDGE — the runtime reminder after EVERY turn that dispatched
/// nothing: TRY AGAIN. DONE is EXPLICIT: the `{"name":"done"}` tool call ends a
/// chain silently and is never nudged; `await` waits. The reminder repeats until
/// the model acts or ends the chain, bounded by [`MAX_SILENT_TURNS`] in a row.
/// Mirrors python `_NO_CALL_NUDGE` (canonical; reinstated 2026-09-01 after
/// repo_map:swift twice stopped mid-task).
const NO_CALL_NUDGE: &str = "[no call was dispatched] Your answer emitted no <call> element, so \
NOTHING ran — a call written in your thinking does not run: write it in your answer, \
after the thinking. If the task is unfinished, emit that call now. If it is finished, \
end the chain explicitly: <call name=\"done\"></call>";

/// Honour where this pass's output goes — the DETACHED routing task. The
/// model's calls are the agent's calls: the task is one the agent's own
/// dispatch spawned (`Kernel::spawn_task`), so every send in it is the agent's
/// and is gated as the agent; `incoming`
/// is the text this turn answered, which decides whether a silent chain turn
/// earns the one no-call nudge.
///
/// `send_timeout` is the AI layer's per-call deadline — the KERNEL defaults
/// nothing; this layer always supplies one, and the model may override it per
/// call with a `timeout` arg in the envelope.
///
/// `model_window` is the routing agent's backend window
/// ([`crate::agent_loop::BackendConfig::model_window`]) — the inline cap a reply
/// body is measured against.
///
/// `call_streams` is every closed call part of the pass, `(stream_id, body)`
/// ([`crate::stream_parts::PartSplitter::closed_calls`]): a dispatched message
/// carries the `stream_id` of the part it was streamed as.
///
/// `cut`: the model stopped with a block open — ONLY A SUCCESSFUL GENERATION
/// DISPATCHES (owner, 2026-10-04), so the pass sends nothing, its closed calls and
/// its malformed-call reports included; it routes as a turn that dispatched nothing.
#[allow(clippy::too_many_arguments)]
pub async fn route_reply(
    self_id: AgentId,
    kernel: Arc<Kernel>,
    text: String,
    client_id: String,
    send_to: Option<String>,
    system_prompt: Option<String>,
    incoming: String,
    send_timeout: f64,
    model_window: i64,
    message_id: Option<i64>,
    dead_turn: bool,
    call_streams: Vec<(String, String)>,
    cut: bool,
    launched: Option<Arc<fantastic_kernel::Signal>>,
) {
    // THE MODEL'S CALLS ARE THE AGENT'S CALLS — gated. The routing task is
    // spawned by the agent's own dispatch through `Kernel::spawn_task`, which
    // carries the dispatch identity into it (a bare tokio spawn would not —
    // measured 2026-09-01, E53_grant): dispatches, the binary carrier, and reply
    // deliveries all attribute to the AI agent.
    route_reply_gated(
        self_id,
        kernel,
        text,
        client_id,
        send_to,
        system_prompt,
        incoming,
        send_timeout,
        model_window,
        message_id,
        dead_turn,
        call_streams,
        cut,
        launched,
    )
    .await
}

/// Fires the route's launch signal however the route ends.
struct FireOnDrop(Option<Arc<fantastic_kernel::Signal>>);

impl Drop for FireOnDrop {
    fn drop(&mut self) {
        if let Some(s) = &self.0 {
            s.fire();
        }
    }
}

/// One EMITTED thing, in emission order — python's `units` tuple
/// `(dest, target, verb, item_index, body)`. `idx` links a dispatched call to its
/// outcome; `body` is pre-decided for the paths that never dispatch (a parse
/// error, a missing target, a missing payload, the `@body` guard, the binary
/// carrier).
struct Unit {
    dest: String,
    target: String,
    verb: String,
    idx: Option<usize>,
    body: Option<String>,
    /// The model's own one-sentence `<title>` for this call. It rides on the
    /// reply's tag and is what REMAINS once the body is stripped from the view,
    /// so it is the call's name forever.
    title: Option<String>,
}

/// One rendered reply block, owed to `dest`. [`deliver_grouped`] composes the tag
/// — including the batch stamp, which lives INSIDE it.
struct Block {
    dest: String,
    pid: String,
    target: String,
    verb: String,
    title: Option<String>,
    body: String,
    /// It carries a dispatched call's outcome (not this layer's own text).
    outcome: bool,
}

#[allow(clippy::too_many_arguments)]
async fn route_reply_gated(
    self_id: AgentId,
    kernel: Arc<Kernel>,
    text: String,
    client_id: String,
    send_to: Option<String>,
    system_prompt: Option<String>,
    incoming: String,
    send_timeout: f64,
    model_window: i64,
    message_id: Option<i64>,
    dead_turn: bool,
    call_streams: Vec<(String, String)>,
    cut: bool,
    launched: Option<Arc<fantastic_kernel::Signal>>,
) {
    // However this route ends, the `send` waiting on its launch is released.
    let _release = FireOnDrop(launched.clone());
    let sp = system_prompt.as_deref();
    // THE MATCH (owner, 2026-10-04): a dispatched message carries the `stream_id`
    // of the part it was streamed as, so a consumer settles its draft on it.
    // Matched by the call's exact body, in order — one reading of the text, not
    // two.
    let mut streams = call_streams;
    // ONLY THE ANSWER ACTS (owner, 2026-10-07): calls written inside thinking are
    // read as thinking — dispatch and the malformed-call report read the answer.
    let answer = crate::call_check::repair(&crate::think::without_thinking(&text));
    let parsed = if cut {
        Vec::new()
    } else {
        terminated_calls(&answer)
    };
    let calls: Vec<crate::tool_parse::ToolCall> = parsed
        .into_iter()
        .map(|(mut call, body)| {
            if let Some(i) = streams.iter().position(|(_, b)| *b == body) {
                call.stream_id = Some(streams.remove(i).0);
            }
            call
        })
        .collect();
    // Everything owed to ONE destination, in the order the model wrote it.
    // Grouping (not a message per call) is the whole point — see below.
    let mut units: Vec<Unit> = Vec::new();

    // An envelope that OPENED and did not parse gets an error, exactly like a call
    // missing its `target_id` does. Reported FIRST and reported even when other
    // calls in the same pass parsed fine — a partially-read turn dispatched half
    // the model's intent, and which half was lost is not something it can infer
    // from the replies it got. Goes to SELF: `reply_to` lives inside the body we
    // could not read, and the documented default for an absent `reply_to` is self
    // anyway. TAGGED like everything else (`[reply to - · - -]`): python tags every
    // unit, so an error block is counted in M and reads like its siblings.
    let malformed = if cut {
        Vec::new()
    } else {
        malformed_calls(&answer)
    };
    for bad in malformed {
        units.push(Unit {
            dest: self_id.as_str().to_string(),
            target: "-".to_string(),
            verb: "-".to_string(),
            idx: None,
            body: Some(bad.body()),
            title: None,
        });
    }
    // CALLS WRITTEN WRONG (owner, 2026-10-07): markup carrying OUR call vocabulary
    // outside every `<call` envelope — `<tool_call><function=call name="send">`,
    // `<send>`, a JSON call — is reported like a compiler error, with the form to
    // write instead (`call_check`). The answer's well-formed calls run as written,
    // and each report says how many did.
    let ran = calls
        .iter()
        .filter(|c| !CONTROL_CALLS.contains(&c.name.as_str()))
        .count();
    let reports = if cut {
        Vec::new()
    } else {
        crate::call_check::check(&answer, ran)
    };
    for report in reports {
        units.push(Unit {
            dest: self_id.as_str().to_string(),
            target: "-".to_string(),
            verb: "-".to_string(),
            idx: None,
            body: Some(report),
            title: None,
        });
    }

    let done_called = calls.iter().any(|c| c.name == "done");
    let await_called = calls.iter().any(|c| c.name == "await");
    let any_calls = !calls.is_empty();
    // ONE TURN, ONE BATCH. Every call the model emitted is dispatched in a
    // SINGLE `kernel.send([...])` — concurrent, side-effect order across
    // calls undefined — and everything owed to one destination arrives as
    // ONE delivered turn.
    //
    // Why whole, never split: replies used to land one per turn, so a turn
    // that fanned out 7 calls spent the next 7 turns collecting its own
    // answers, and every question asked in between was answered turns later
    // by something older. Measured 2026-08-25 (`live_subai_langgraph`): the
    // model re-asked `list_agents` five times and the run died with nothing
    // built. Anthropic documents the split shape as the anti-pattern
    // ("separate user messages reduce parallel tool use"), OpenAI 400s on a
    // missing tool result, LangGraph's ToolNode collects every ToolMessage
    // before the agent runs again. The model never sees a PARTIAL batch, so
    // it never re-asks for the missing half.
    let mut items: Vec<Item> = Vec::new();
    // What each item was sent — its target as the model wrote it and its payload —
    // for the origin line of a digested reply and the grants-cache refresh.
    let mut payloads: Vec<(String, Value)> = Vec::new();

    // ONE COPY PER RECIPIENT. A `<to>` list is not a group address the kernel
    // knows about — there is no such thing; every send reaches exactly one
    // agent. What the list buys is that each copy carries the WHOLE list, so a
    // recipient can see who else heard it and reply to the same group instead of
    // forking a private conversation the others never see.
    let calls = fan_out(calls, message_id);
    // THE ANSWER to an inquiry this chain received goes to the asker now, under the
    // inquiry's pid; an answer with nothing to answer, or a placeholder, is told so.
    for call in calls.iter().filter(|c| c.name == "inquiry_answer") {
        if let Some(err) = answer_inquiry(&kernel, self_id.as_str(), &client_id, call).await {
            units.push(Unit {
                dest: self_id.as_str().to_string(),
                target: "-".to_string(),
                verb: "-".to_string(),
                idx: None,
                body: Some(err),
                title: None,
            });
        }
    }

    let mut doc_args: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for call in calls {
        if CONTROL_CALLS.contains(&call.name.as_str()) {
            // CONTROL CALLS, not dispatches. `done` ends the chain; `await`
            // stops the turn until something lands; `inquiry_answer` hands this
            // turn's answer back to whoever sent the inquiry (the `inquiry` verb
            // reads it). None is a send, none is an error, none is ever nudged.
            continue;
        }
        let args = &call.arguments;
        let chosen: Option<&Value> = match args.get("target_id") {
            Some(v) if py_truthy(v) => Some(v),
            _ => args.get("target"),
        };
        let target: Option<&str> = chosen
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // `reply_to` DEFAULTS TO SELF. An omitted key means the model gets its
        // own answer back, not that the answer is thrown away.
        //
        // The old default was fire-and-forget, and it made the one failure a
        // model can never debug: gemma4:12b once left the key out of an
        // otherwise perfect call, the reflect ran, its result was dropped, and
        // the chain died at pass 1 with no reply, no error, and nothing to see.
        // From inside the model that is indistinguishable from being finished.
        //
        // Stopping is still the default and still costs nothing — it is
        // emitting NO call. A dropped key now costs a wasted turn instead of
        // the whole task, and discarding a reply became something you must SAY
        // (`reply_to: null` / "none"), which is the right way round.
        // The tag grammar types a bare number (`<reply_to>1001</reply_to>` → 1001):
        // a number is an id the model wrote, never "discard" — it is checked like
        // any id below (python, measured 2026-10-11, e2e chat_users).
        let reply_to: Option<String> = match args.get("reply_to") {
            Some(v) => match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(py_str(&Value::Number(n.clone()))),
                _ => None,
            }
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| !matches!(s.to_lowercase().as_str(), "none" | "null" | "nobody"))
            .map(str::to_string),
            None => Some(self_id.as_str().to_string()),
        };
        let reply_to = reply_to.map(|r| {
            if r == "self" {
                self_id.as_str().to_string()
            } else {
                r
            }
        });

        let Some(target) = target else {
            if let Some(rt) = reply_to {
                let body = json!({"error": NO_TARGET_ERROR, "you_sent": you_sent(args)});
                units.push(Unit {
                    dest: rt,
                    target: "-".to_string(),
                    verb: "-".to_string(),
                    idx: None,
                    body: Some(crate::tool_parse::render_reply(&body)),
                    title: None,
                });
            }
            continue;
        };
        // A REPLY ADDRESSED TO NOBODY IS LOST. `reply_to` is where this call's
        // answer goes, never an argument the target sees; one naming no agent made
        // the delivery fail on that missing agent's inbox, so nobody saw it and the
        // model never learned what its call did (python, measured 2026-10-11, e2e
        // chat_users). Nothing is dispatched; the model is told. A peer address
        // (`<guid>|<agent>`) is the peer's to resolve.
        if let Some(rt) = reply_to.as_deref() {
            if rt != self_id.as_str() && !rt.contains('|') && kernel.resolve_target(rt).is_none() {
                let verb = match args.get("payload") {
                    Some(Value::Object(p)) => p.get("type").map_or("?".to_string(), py_str),
                    _ => "-".to_string(),
                };
                let body = json!({
                    "error": format!(
                        "reply_to={} names no agent, so nothing was dispatched to {}. \
                         `reply_to` is where this call's reply goes (left out: back to \
                         you) and never reaches the target — an argument of the \
                         target's verb needs the name its docs give.",
                        py_repr(rt),
                        py_repr(target)
                    ),
                    "target": target,
                    "reply_to": rt,
                });
                units.push(Unit {
                    dest: self_id.as_str().to_string(),
                    target: target.to_string(),
                    verb,
                    idx: None,
                    body: Some(crate::tool_parse::render_reply(&body)),
                    title: None,
                });
                continue;
            }
        }
        // AN EMPTY PAYLOAD IS NO PAYLOAD. The grammar always produces the key
        // (every child that is not routing IS the payload), so `{}` is exactly
        // what a send with no verb looks like — and dispatching it reaches the
        // kernel as `payload.type required`, an error about OUR shape rather
        // than about the verb the model left out.
        let Some(Value::Object(payload)) = args
            .get("payload")
            .filter(|p| p.as_object().is_some_and(|m| !m.is_empty()))
        else {
            if let Some(rt) = reply_to {
                let body = json!({
                    "error": no_payload_error(target),
                    "target": target,
                    "you_sent": you_sent(args),
                });
                units.push(Unit {
                    dest: rt,
                    target: target.to_string(),
                    verb: "-".to_string(),
                    idx: None,
                    body: Some(crate::tool_parse::render_reply(&body)),
                    title: None,
                });
            }
            continue;
        };

        let mut payload = payload.clone();
        // A SELF-CALL INHERITS ITS CONVERSATION. `grep`, `pull` and `recall` are
        // per-client, and a call the MODEL makes belongs to the client whose
        // turn it is — python 2026-09-06: without this, every model-issued
        // `grep` in three e2e runs returned `total=0` against a store full of
        // matches, because the call landed on the default client.
        if target == self_id.as_str() && !payload.contains_key("client_id") {
            payload.insert("client_id".to_string(), json!(client_id));
        }
        // python `payload.get("type", "?")`, then `str()` into the reply tag: a
        // NON-STRING type tags the reply with its own text (`… a 42 …`,
        // `… a True …`, `… a {'a': 'x'} …`), and `?` means the key was ABSENT.
        // `as_str().unwrap_or("?")` printed `?` for every one of them.
        let verb = match payload.get("type") {
            Some(v) => py_str(v),
            None => "?".to_string(),
        };
        // A ROUTING NAME THE VERB ALSO TAKES IS LOST (owner, 2026-10-11). `title`,
        // `timeout`, `reply_to`, … are this call's own fields — lifted out, never
        // sent to the target. A verb that documents an argument of the same name can
        // never receive it from a model call (python, measured: a knowledge store's
        // `create(title)` failed on every call, the model retried 8 times and blamed
        // the store). The target's `docs` names the verb's arguments; a clash refuses
        // the call, loud.
        if let Some(vt) = payload.get("type").and_then(Value::as_str) {
            let used: Vec<&str> = crate::tool_parse::grammar::ROUTING
                .iter()
                .copied()
                .filter(|k| *k != "target_id" && args.contains_key(*k))
                .collect();
            if !used.is_empty() {
                let clash = routing_clash(&kernel, target, vt, &used, &mut doc_args);
                if !clash.is_empty() {
                    let names = clash.join(", ");
                    let body = json!({
                        "error": format!(
                            "{names} on this call {} call routing, never sent to the target \
                             — but {target}'s `{vt}` documents an argument named {names}, \
                             which no call can deliver; nothing was dispatched. Call it \
                             without that argument, or have the verb's owner rename it.",
                            if clash.len() == 1 { "is" } else { "are" }
                        ),
                        "target": target,
                        "clash": clash,
                    });
                    units.push(Unit {
                        dest: self_id.as_str().to_string(),
                        target: target.to_string(),
                        verb: verb.clone(),
                        idx: None,
                        body: Some(crate::tool_parse::render_reply(&body)),
                        title: None,
                    });
                    continue;
                }
            }
        }

        // THE `bytes` ARGUMENT IS RAW BYTES, NOT A JSON STRING (A13.19).
        // Python's parser returns `raw.encode("utf-8")` for the one argument
        // named `bytes`, so the value that reaches a handler is TYPED — which
        // is why `file_bridge.write_stream` can refuse `bytes: "abc"` from a
        // wire client (`isinstance(data, bytes)`) while a model's `<bytes>`
        // document still writes. `serde_json::Value` has no bytes variant, so
        // the item's BINARY CHANNEL is that type: the key leaves the JSON
        // header and the document rides beside it, raw. Carrying it as a JSON
        // string instead made the two indistinguishable, and rust wrote `abc`
        // where python refused.
        let document = match payload.get("bytes") {
            Some(Value::String(s)) => Some(s.clone().into_bytes()),
            _ => None,
        };
        if document.is_some() {
            payload.shift_remove("bytes");
        }
        // THE ASKER'S THREAD rides an inquiry, so the answer that comes back later
        // (fully async) lands where this chain is working.
        if payload.get("type").and_then(Value::as_str) == Some("inquiry") {
            payload.insert("reply_client_id".to_string(), json!(client_id));
        }
        payloads.push((target.to_string(), Value::Object(payload.clone())));
        let mut item = Item::new(AgentId::from(target), Value::Object(payload.clone()));
        if let Some(doc) = document {
            item = item.with_binary_key("bytes", doc);
        }
        // EVERY dispatched call carries the layer's deadline. The KERNEL
        // defaults nothing, so a call to a handler that never returns used to
        // hang the whole batch forever here — no reply, no timeout block, the
        // chain simply stopped (python has always set it; rust/swift did not
        // until 2026-09-02). Set BEFORE the model's own `timeout` arg, which
        // overrides it; the kernel validates that value, so a bad one settles
        // as a refusal the model can read.
        item = item.with_timeout(send_timeout);
        // The model may NAME the execution (dedup/join across retries) and may
        // bound its own call. FORWARDED RAW (C38) — the kernel is what
        // validates all three and produces python's exact refusals; this used
        // to coerce with `.and_then(Value::as_str/as_f64/as_u64)`, which
        // silently DROPPED anything that did not already match the expected
        // shape (an int `exec_id` never attempted the join python performs; a
        // malformed `timeout` like `"soon"` vanished, defaulting to
        // `send_timeout`, instead of settling the kernel's `bad_arg` refusal).
        // `exec_id`, not `id`: arguments are INLINE now, so a bare `<id>` would
        // collide with the many verbs whose payload has one (`delete_agent`,
        // `update_agent`, …). The parser keeps the name at the call level; here
        // it becomes the item's execution name.
        if let Some(id) = args.get("exec_id") {
            item = item.with_raw_id(id.clone());
        }
        if let Some(t) = args.get("timeout") {
            item = item.with_raw_timeout(t.clone());
        }
        if let Some(r) = args.get("retries") {
            item = item.with_raw_retries(r.clone());
        }
        items.push(item);
        // Fire-and-forget: it acted, nobody awaits the answer. Deliberately
        // NOT counted in M — the model is owed a count of what it will
        // actually receive.
        if let Some(rt) = reply_to {
            units.push(Unit {
                dest: rt,
                target: target.to_string(),
                verb,
                idx: Some(items.len() - 1),
                body: None,
                title: clean_title(args.get("title")),
            });
        }
    }

    let had_items = !items.is_empty();
    let outcomes = if items.is_empty() {
        Vec::new()
    } else {
        // BRACKETED so `await` can see it: while this batch is out, the agent has
        // something in flight, and that is the honest answer to "is anything
        // coming?" — the alternative the model reaches for otherwise is
        // re-issuing the call, which only lengthens the queue that delays it.
        inflight_bump(self_id.as_str(), 1);
        // LAUNCHED = every routed call has reached its target; the `send` that
        // spawned this route returns only then (swift `onBatchLaunched`).
        let out = match &launched {
            Some(sig) => {
                let sig = Arc::clone(sig);
                let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || sig.fire());
                fantastic_kernel::ON_BATCH_LAUNCHED
                    .scope(Some(hook), kernel.send(items))
                    .await
            }
            None => kernel.send(items).await,
        };
        inflight_bump(self_id.as_str(), -1);
        out
    };

    // AN ACCEPTED INQUIRY IS OWED AN ANSWER — counted until a message under its pid
    // arrives (`settle_owed`). Python: the same loop after `kernel.send(items)`.
    for ((_, sent), out) in payloads.iter().zip(outcomes.iter()) {
        let accepted = out.result.get("accepted").is_some_and(py_truthy);
        if sent.get("type").and_then(Value::as_str) == Some("inquiry") && accepted {
            let pid = out.pid.map_or_else(
                || py_str(out.result.get("re").unwrap_or(&Value::Null)),
                |p| p.0.to_string(),
            );
            awaiting()
                .lock()
                .expect("awaiting poisoned")
                .entry(self_id.as_str().to_string())
                .or_default()
                .insert(pid);
        }
    }

    // The result — INCLUDING an error — goes back. A call that failed silently
    // is the one thing a model can never debug.
    //
    // PID first, then target+verb. Target+verb alone is NOT unique: with two
    // calls in flight to one agent `[reply to kernel_state set]` is unmatchable
    // — measured 2026-08-07 (opus re-issued both deletes, then theorised
    // "duplicate delivery" about its own retries) and again 2026-08-20 (the
    // model: "the reply queue is running ahead of me", then re-polled forever).
    // The PID is the same id `ps`/`cancel` use, so a reply names a row the
    // model can act on.
    let meta = crate::helpers::agent_meta(&self_id, &kernel);
    let cap = crate::context::inline_cap(&meta, model_window);
    // THE CAP IS PER MESSAGE, not per reply (owner, 2026-10-07): every reply of a
    // batch lands in ONE message to its addressee, so the replies share the cap.
    // A reply that would push its batch past it is held like an oversized one.
    // Measured: two greps, each under the 434-token cap, arrived as one 967-token
    // message at a 4.5k window — the next turn could not fit it (`too_small`).
    let mut used: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let blocks: Vec<Block> = units
        .into_iter()
        .map(|u| {
            let outcome = u.idx.is_some();
            let so_far = used.get(u.dest.as_str()).copied().unwrap_or(0);
            let (pid, body) = match u.idx {
                Some(i) => {
                    let out = &outcomes[i];
                    let pid = out
                        .pid
                        .map(|t| t.0.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let shown = reply_as_text(out);
                    let pointed = point_to_docs(&u.target, &u.verb, &shown);
                    let mut body =
                        crate::tool_parse::render_reply(pointed.as_ref().unwrap_or(&shown));
                    // Only docs bodies are cut later, so only they carry the notice, and
                    // are HELD under their pid for the model's own `grep`. A reflect is
                    // MUTABLE state and stays whole (owner, 2026-10-06); a newer
                    // reflect of the same agent supersedes it (dedupe).
                    let docs_body = u.verb == "docs" && shown.get("of").is_some();
                    if let (true, Some(p), Some(verb)) = (
                        docs_body,
                        out.pid,
                        example_verb(&body).filter(|_| docs_body),
                    ) {
                        crate::memory::store_reply(&self_id, &kernel, p.0, &body);
                        body = docs_head(p.0, &verb) + &body;
                    }
                    // FULL ASYNC. A reply is DATA, not a question — the model
                    // needs to be able to SEE it, not be forced to swallow it
                    // whole as its next turn. Anything over the inline cap is
                    // held in RAM under its pid and replaced by its digest + its
                    // ORIGIN line. SYNC OR ASYNC ALIKE (decided 2026-09-26): a
                    // body is never persisted; under the cap it is inline.
                    if let Some(p) = out.pid {
                        let over = so_far
                            + crate::context::estimate_one(
                                &json!({"role": "user", "content": body}),
                            )
                            > cap;
                        if over && crate::memory::store_reply(&self_id, &kernel, p.0, &body) {
                            // CODE POINTS: python prints `len(body)`.
                            let size = body.chars().count();
                            let sent = payloads.get(i).map_or(&Value::Null, |(_, p)| p);
                            // THE STUB IS THE ORIGIN. The body is held in RAM for
                            // this incarnation only and never persisted; what
                            // survives is the one line saying how to get it again.
                            body = format!(
                                "(settled, {size} bytes — NOT inlined, held in memory only \
                                 until this agent restarts. origin: {}. To find a line in it: \
                                 send yourself <type>grep</type> <query>substring</query> \
                                 <ids><item>{}</item></ids>)",
                                origin_line(&out.result, &u.target, &u.verb, sent),
                                p.0
                            );
                        }
                    }
                    (pid, body)
                }
                None => ("-".to_string(), u.body.unwrap_or_default()),
            };
            *used.entry(u.dest.to_string()).or_insert(0) +=
                crate::context::estimate_one(&json!({"role": "user", "content": body}));
            Block {
                dest: u.dest,
                pid,
                target: u.target,
                verb: u.verb,
                title: u.title,
                body,
                outcome,
            }
        })
        .collect();

    // THE TAIL BELOW IS PYTHON'S ORDER, step for step (core.py:1160-1176), and
    // every step of it is load-bearing.
    //
    // EVERY CALL-LESS TURN IS REMINDED — no streak, no cap (python `_route_reply`:
    // "No streak, no cap: both exits are above this branch and both already
    // returned"). The two honest ends are ABOVE this branch and both return:
    // `done` finishes and `await` waits. A bound here was rust-only, and what it
    // bounded was the reminder rather than the work: on the 25th silent turn the
    // chain was dropped in silence, which is the exact failure the reminder exists
    // to prevent.
    //
    // THE GATE IS `not calls and not units`, not `no blocks`: a turn whose calls
    // all carried `reply_to:null` dispatched real work and is owed no reply, and
    // rust nudged it anyway until 2026-09-02 — telling the model "your turn emitted
    // no <call> element" about a turn that did, and then handing that turn's
    // text to `send_to` on top.
    //
    // An inquiry turn was the OTHER legitimate silent end (emit + stop; the human's
    // answer resumes the chain) — nudging it would stomp the wait.
    let any_blocks = !blocks.is_empty();
    // THE SILENT STREAK of this chain (one per agent + client thread): turns in a
    // row that dispatched nothing. Any call — `done` and `await` included — ends
    // the streak; a turn whose only output was BROKEN calls ran nothing and does
    // not (see MAX_SILENT_TURNS).
    let chain = (self_id.as_str().to_string(), client_id.clone());
    if any_calls {
        silent().lock().expect("silent poisoned").remove(&chain);
    }
    // EVERY CALL-LESS TURN IS NUDGED, chain or not (user, 2026-09-09: "all is
    // tool calls"). This used to fire only when the incoming text was itself a
    // reply or a nudge, so a FIRST turn of pure prose from a person ended the
    // work silently — the model had decided on a call, written it in thought,
    // and never emitted it.
    let _ = &incoming;
    let chained = true;
    // AN INQUIRY IS A CALL NOW, so there is no silent-turn exemption: a turn that
    // consults someone DISPATCHES and is never silent. This used to scan the
    // model's own prose for the old `{"type":"question"}` line — a text convention
    // had to recognise, with the human living outside it entirely.
    let nudging = !any_calls && !any_blocks && chained;
    // Only broken calls: their reports are the next turn — unless this chain has
    // now been silent MAX_SILENT_TURNS in a row, and gives up.
    if !any_calls
        && any_blocks
        && silent_turn(&kernel, &self_id, &client_id, send_to.as_deref(), sp).await
    {
        return;
    }
    if any_blocks {
        deliver_grouped(
            &kernel,
            self_id.as_str(),
            blocks,
            &client_id,
            sp,
            message_id,
        )
        .await;
    }
    if done_called {
        // AN INQUIRY THIS CHAIN NEVER ANSWERED is closed for its asker, saying so —
        // the asker must not wait on an answer that will not come.
        let mut body = Map::new();
        body.insert(
            "error".to_string(),
            json!(format!(
                "{} ended its run without answering",
                self_id.as_str()
            )),
        );
        close_inquiry(&kernel, self_id.as_str(), &client_id, body).await;
        // Explicitly finished — nothing more runs, nothing reminds. A caller's
        // `send_to` hand-off is the one thing that still applies: the finished
        // text is what it asked to have delivered.
        if !any_blocks {
            if let Some(to) = send_to.as_deref() {
                deliver_soon(&kernel, to, &text, &client_id, sp, false);
            }
        }
        return;
    }
    if await_called {
        // EXPLICIT WAITING. The model stops and nothing reminds it: the next
        // message that lands wakes it. That is the honest alternative to
        // re-issuing a call it has not seen answered — this is an ASYNC system,
        // and a reply in flight is on its way, not lost.
        //
        // PENDING is three summands, and all three are needed: this agent's own
        // in-flight routing turns, the messages already queued for it, and the
        // turn another message may ALREADY be running under the lock. Routing
        // happens AFTER the turn released that lock, so "queued" alone would
        // read empty at exactly the moment something is about to arrive.
        let st = state_for(&self_id);
        let queued = st.queue.lock().expect("queue poisoned").len() as u64;
        let running = u64::from(st.lock.try_lock().is_err());
        // ...and the answers its inquiries are still owed (fully async).
        let owed = awaiting()
            .lock()
            .expect("awaiting poisoned")
            .get(self_id.as_str())
            .map_or(0, HashSet::len) as u64;
        let pending = inflight_count(self_id.as_str()) + queued + running + owed;
        // TODO (owner, 2026-10-05) — STALL DETECTION. A chain can stop with no error: calls
        // dispatched, then nothing (subai_graph:rust, 2026-09-03 and 2026-10-05). Three shapes:
        // (1) a call never settles (no timeout by default, its task stays in `ps`); (2) every
        // call settled but the reply batch never reached this agent's conversation; (3) the
        // reply landed but no turn started. This agent KNOWS it is awaiting `pending` work:
        // if nothing changes for T seconds, emit status `stalled` naming the calls not settled
        // / not delivered, so a CLI / canvas / harness shows it like any status.
        if pending > 0 || had_items {
            let mut detail = Map::new();
            detail.insert("reason".to_string(), json!("awaiting"));
            detail.insert("pending".to_string(), json!(pending));
            emit_status(&kernel, &st, &self_id, &client_id, "done", detail).await;
            return;
        }
        // Nothing to wait for: say so, as a turn, so the model acts instead of
        // waiting forever on an empty inbox.
        deliver_soon(
            &kernel,
            self_id.as_str(),
            AWAIT_REFUSED,
            &client_id,
            sp,
            true,
        );
        return;
    }
    if nudging {
        // TRY AGAIN — and a DEAD turn (only thinking, discarded unstored) is the
        // same silence, rerolled the same way.
        //
        // BOUNDED (owner, 2026-09-30): MAX_SILENT_TURNS in a row and the chain
        // GIVES UP instead of reminding again — the caller is told, loudly, that
        // the model could not answer. A person sees it in their view; a parent
        // that asked for the result by `send_to` gets it as a turn; a clock's tick
        // has nobody reading, so the status event is the record, and the next
        // tick starts a fresh chain with a fresh count.
        if silent_turn(&kernel, &self_id, &client_id, send_to.as_deref(), sp).await {
            return;
        }
        let nudge = if dead_turn {
            format!("{DEAD_TURN_LINE}{NO_CALL_NUDGE}")
        } else {
            NO_CALL_NUDGE.to_string()
        };
        deliver_soon(&kernel, self_id.as_str(), &nudge, &client_id, sp, true);
        return;
    }
    if any_calls {
        return;
    }
    // No call of the model's own, so the CALLER's one-shot handoff applies: hand
    // this pass's TEXT to `send_to`, once. It must not propagate — a caller cannot
    // spend the model's turns for it.
    if let Some(to) = send_to.as_deref() {
        deliver_soon(&kernel, to, &text, &client_id, sp, false);
    }
}

/// The calls that are not dispatches: they end, park or answer the chain.
const CONTROL_CALLS: [&str; 3] = ["done", "await", "inquiry_answer"];

/// Count one silent turn of the chain (a turn that ran nothing). At
/// [`MAX_SILENT_TURNS`] in a row the chain GIVES UP — the status says so, an open
/// inquiry is closed, a `send_to` is told — and `true` is returned. Python
/// `_silent_turn`.
async fn silent_turn(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    client_id: &str,
    send_to: Option<&str>,
    sp: Option<&str>,
) -> bool {
    let chain = (self_id.as_str().to_string(), client_id.to_string());
    let streak = {
        let mut m = silent().lock().expect("silent poisoned");
        let e = m.entry(chain.clone()).or_insert(0);
        *e += 1;
        *e
    };
    if streak < MAX_SILENT_TURNS {
        return false;
    }
    silent().lock().expect("silent poisoned").remove(&chain);
    let gave_up = gave_up_text();
    let st = state_for(self_id);
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("gave_up"));
    detail.insert("error".to_string(), json!(gave_up));
    detail.insert("silent_turns".to_string(), json!(streak));
    emit_status(kernel, &st, self_id, client_id, "done", detail).await;
    let mut body = Map::new();
    body.insert("error".to_string(), json!(gave_up));
    close_inquiry(kernel, self_id.as_str(), client_id, body).await;
    if let Some(to) = send_to {
        deliver_soon(kernel, to, &gave_up, client_id, sp, false);
    }
    true
}

/// ONE delivered message per destination: blocks joined by a blank line, each
/// tagged `[reply to <pid> · <target> <verb>]` — with ` · N of M in batch <token>`
/// composed INSIDE that tag when the destination is owed more than one.
///
/// INSIDE, not a trailing line. The stamp rode on its own `[N of M in batch tok]`
/// line after the body until 2026-09-02, which is neither what the shared prompt
/// teaches nor what python's history trimmers can read (`_REPLY_TAG` /
/// `_BLOCK_SPLIT` match the tag at the START of a block) — so a rust transcript
/// looked untrimmable to python and a cross-runtime byte-diff failed on every
/// multi-call turn.
///
/// The stamp is for CORRELATION and COMPLETENESS, not "the rest are still in
/// flight" — the batch arrives whole. A model that made 2 calls and got 1 back
/// could not tell "still running" from "answered the wrong one"; that killed
/// three runs (opus 08-07, the model 08-20, the model 08-24).
async fn deliver_grouped(
    kernel: &Arc<Kernel>,
    self_id: &str,
    blocks: Vec<Block>,
    client_id: &str,
    sp: Option<&str>,
    message_id: Option<i64>,
) {
    // Destination order = first appearance, so the model reads them in the order
    // it wrote the calls.
    let mut order: Vec<String> = Vec::new();
    let mut by_dest: HashMap<String, Vec<Block>> = HashMap::new();
    for b in blocks {
        if !by_dest.contains_key(&b.dest) {
            order.push(b.dest.clone());
        }
        by_dest.entry(b.dest.clone()).or_default().push(b);
    }
    // A FRESH token per TURN, shared across destinations: it names the batch, and
    // a model reading two destinations' messages can see they came from one turn.
    // THE BATCH TOKEN IS THE TURN'S OWN ID when one exists (python), so every
    // `↑` and every `batch` on a reply points at a row the model can see.
    let token = message_id.map_or_else(mint_batch_token, |t| t.to_string());
    // EVERY DESTINATION'S INCARNATION, READ NOW — before any delivery is spawned.
    // python reads `born` for all of them in one synchronous loop
    // (`_deliver_soon`), so a target that reboots while a sibling delivery is
    // still thinking is still judged against the incarnation this turn addressed.
    let born: HashMap<String, u64> = order
        .iter()
        .map(|d| {
            (
                d.clone(),
                crate::state::incarnation(&AgentId::from(d.as_str())),
            )
        })
        .collect();
    for dest in order {
        let parts = by_dest.remove(&dest).unwrap_or_default();
        let m = parts.len();
        // A batch to SELF made only of this layer's own text is a notice; one that
        // carries any call's outcome is a reply.
        let notice = dest == self_id && !parts.iter().any(|b| b.outcome);
        let body = parts
            .iter()
            .enumerate()
            .map(|(i, b)| {
                // The ONE line a reply is known by, forever — in the durable
                // store, in `recall` pages, and as the stub a stripped body
                // leaves behind. Optional segments append in THIS order:
                //     [reply to <pid> · <target> <verb> · "<title>" · N of M in batch <b>]
                let mut tag = format!("[reply to {} \u{b7} {} {}", b.pid, b.target, b.verb);
                if let Some(t) = &b.title {
                    tag.push_str(&format!(" \u{b7} \"{t}\""));
                }
                if let Some(t) = message_id {
                    tag.push_str(&format!(" \u{b7} \u{2191}{t}"));
                }
                if m > 1 {
                    tag.push_str(&format!(" \u{b7} {} of {m} in batch {token}]", i + 1));
                } else {
                    tag.push(']');
                }
                format!("{tag}\n{}", b.body)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        deliver_soon_with_born(
            kernel,
            &dest,
            &body,
            client_id,
            sp,
            born.get(&dest).copied().unwrap_or_default(),
            notice,
        );
    }
}

/// The routing fields of this call that `target`'s `verb` also documents as
/// arguments — read from the kernel's own `docs` (the signature the model reads),
/// in process: no dispatch, no ledger row, once per (target, verb) per batch. A
/// target this kernel cannot document (a peer's agent, an unknown verb) documents
/// nothing here: the call goes on, and its own reply says what happened. Python
/// `_routing_clash`.
fn routing_clash(
    kernel: &Arc<Kernel>,
    target: &str,
    verb: &str,
    used: &[&str],
    cache: &mut HashMap<(String, String), HashSet<String>>,
) -> Vec<String> {
    static DOC_ARG: OnceLock<regex::Regex> = OnceLock::new();
    let re = DOC_ARG.get_or_init(|| {
        regex::Regex::new(r"(?:\bargs:|,)\s*([A-Za-z_][A-Za-z0-9_]*)\s*:").expect("doc arg regex")
    });
    let names = cache
        .entry((target.to_string(), verb.to_string()))
        .or_insert_with(|| {
            let res =
                fantastic_kernel::docs::docs_reply(kernel, &json!({"of": target, "verb": verb}));
            let sig = res
                .get("verbs")
                .and_then(|v| v.get(verb))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            re.captures_iter(&sig).map(|c| c[1].to_string()).collect()
        });
    used.iter()
        .filter(|k| names.contains(**k))
        .map(|k| (*k).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_payload_error_uses_python_repr() {
        // `{target!r}`: python's repr picks the quote and escapes the controls.
        assert!(no_payload_error("it's").starts_with("send to \"it's\" carried"));
        assert!(no_payload_error("C:\\notes").starts_with("send to 'C:\\\\notes' carried"));
        assert!(no_payload_error("tab\there").starts_with("send to 'tab\\there' carried"));
    }

    #[test]
    fn clean_title_matches_python() {
        // Runs of `]`/`\r`/`\n` collapse to ONE space, then python's strip
        // (White_Space + `\x1c`–`\x1f`), then 140 CODE POINTS.
        assert_eq!(
            clean_title(Some(&json!("a]]\r\nb"))),
            Some("a b".to_string())
        );
        assert_eq!(
            clean_title(Some(&json!("  a \u{1c}"))),
            Some("a".to_string()),
            "python strips the information separators; `trim()` leaves them"
        );
        // 150 family emoji cut to 140 CODE POINTS (not graphemes, not bytes).
        let long = "\u{1f468}".repeat(150);
        let cut = clean_title(Some(&json!(long))).unwrap();
        assert_eq!(cut.chars().count(), 140);
        // Non-string / strip-empty is no title at all.
        assert_eq!(clean_title(Some(&json!(7))), None);
        assert_eq!(clean_title(Some(&json!("\u{1d}\n"))), None);
        assert_eq!(clean_title(None), None);
    }

    /// The model-facing strings are PROMPTS, not diagnostics: a model reads them
    /// and retries from them, and all four runtimes must say the same words. Pinned
    /// as LITERAL TEXT so "tidying" one of them shows up as a failing test rather
    /// than as silent cross-runtime drift.
    #[test]
    fn the_model_facing_error_strings_are_verbatim() {
        assert_eq!(
            NO_TARGET_ERROR,
            "no target_id, so nothing was dispatched — a send must name the AGENT \
             it goes to. Shape: <call name=\"send\"><target_id>agent</target_id>\
             <type>verb</type></call>. Send `kernel` <type>reflect</type> for the \
             list of agent ids."
        );
        assert_eq!(
            no_payload_error("totals"),
            "send to 'totals' carried no payload, so nothing was dispatched. A call \
             names its verb in `<type>`, e.g. <type>reflect</type>."
        );
        assert_eq!(
            NO_CALL_NUDGE,
            "[no call was dispatched] Your answer emitted no <call> element, so \
             NOTHING ran — a call written in your thinking does not run: write it in \
             your answer, after the thinking. If the task is unfinished, emit that call \
             now. If it is finished, end the chain explicitly: <call name=\"done\"></call>"
        );
        // The fourth is composed by the parser, which OWNS the wording (see
        // `MalformedCall::error` and the literal pin in
        // `tool_parse::tests_malformed`); what this asserts is that the router
        // delivers THAT, verbatim, with nothing reworded on the way through.
        let bad =
            crate::tool_parse::malformed_calls("<call name=\"send\"><type>reflect</type></call>");
        assert_eq!(bad.len(), 1, "one opened, unparseable envelope: {bad:?}");
        assert!(
            bad[0]
                .error
                .starts_with("call NOT dispatched: a send names its target"),
            "verbatim prefix: {:?}",
            bad[0].error
        );
        assert!(
            bad[0]
                .error
                .ends_with(&format!(". Nothing ran. {}", crate::tool_parse::HINT)),
            "verbatim hint: {:?}",
            bad[0].error
        );
        assert!(
            bad[0].body().starts_with(&bad[0].error),
            "the body the router delivers opens with that exact sentence"
        );
    }

    /// `target` is accepted beside `target_id`, and the fall-through is python's
    /// `or`: a falsy `target_id` yields to `target`, a truthy non-string one does
    /// not (it earns the shape error instead of being silently re-read).
    #[test]
    fn target_fallthrough_matches_python_truthiness() {
        let f = |v: Value| {
            let obj = v.as_object().unwrap().clone();
            let chosen: Option<&Value> = match obj.get("target_id") {
                Some(v) if py_truthy(v) => Some(v),
                _ => obj.get("target"),
            };
            chosen
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        assert_eq!(f(json!({"target_id": "a"})), Some("a".to_string()));
        assert_eq!(f(json!({"target": " b "})), Some("b".to_string()));
        assert_eq!(f(json!({"target_id": "", "target": "b"})), Some("b".into()));
        assert_eq!(
            f(json!({"target_id": null, "target": "b"})),
            Some("b".into())
        );
        assert_eq!(f(json!({"target_id": 7, "target": "b"})), None);
        assert_eq!(f(json!({"reply_to": "self"})), None);
    }

    /// python `render_reply`: a reply dict holding bytes renders them decoded
    /// utf-8 (replace) AT THEIR KEY — any verb's, `read_stream`'s `bytes`
    /// included; a reply with no body renders as it is.
    #[test]
    fn a_reply_body_reads_as_utf8_text_at_its_slot() {
        use fantastic_kernel::outcome::{Outcome, Status};
        use fantastic_kernel::{Blob, Step};
        let read = Outcome::new(
            None,
            Status::Completed,
            json!({"path": "a.txt", "offset": 0, "bytes": null, "eof": true}),
        )
        .with_body(Some(Blob::key(
            "bytes",
            b"line one\nline \xff two".to_vec(),
        )));
        assert_eq!(
            crate::tool_parse::render_reply(&reply_as_text(&read)),
            "<path>a.txt</path>\n<offset>0</offset>\n<bytes>\nline one\nline \u{fffd} two\n</bytes>\n<eof>true</eof>"
        );
        let nested = Outcome::new(
            None,
            Status::Completed,
            json!({"seq": 3, "out": {"chunk": null}}),
        )
        .with_body(Some(Blob::new(
            vec![Step::from("out"), Step::from("chunk")],
            b"red".to_vec(),
        )));
        assert_eq!(
            *reply_as_text(&nested),
            json!({"seq": 3, "out": {"chunk": "red"}})
        );
        let plain = Outcome::new(None, Status::Completed, json!({"seq": 3}));
        assert_eq!(*reply_as_text(&plain), json!({"seq": 3}));
    }
}
