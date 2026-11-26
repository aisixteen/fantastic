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

use crate::events::emit_status;
use crate::helpers::mint_batch_token;
use crate::history_view::point_to_docs;
use crate::state::state_for;
use crate::tool_parse::terminated_calls;
use fantastic_bundle::{py_repr, py_repr_value, py_strip};
use fantastic_kernel::outcome::Status;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// What a call that named a VERB but no agent gets told.
///
/// A PROMPT, not a diagnostic — it is read by a model, so it names the SHAPE and
/// not just the fault, and it is byte-identical across runtimes (pinned by a
/// literal-text test). [`crate::tool_parse`] already translates a verb-named tool
/// into a send whenever the args say WHERE to send it; what reaches here named a
/// verb and no agent, and that is a gap only the model can close — guessing a
/// target would dispatch a write to whichever agent we picked.
///
/// A bare fault (e.g. for `{"name":"reflect","arguments":{"detail":"full"}}`)
/// tells the model nothing about how to retry, so it re-derives the envelope from
/// the system prompt instead. Every other error on this path states the fix; so
/// does this one.
pub const NO_TARGET_ERROR: &str = "no target_id, so nothing was dispatched — a \
send must name the AGENT it goes to. Shape: <call name=\"send\"><routing><target_id>agent\
</target_id></routing><payload><type>verb</type></payload></call>. Send `kernel` <type>reflect</type> for the \
list of agent ids.";

/// What a send with no payload gets told, `{target}` filled in.
///
/// NO PAYLOAD, NO SEND. Dispatching nothing reaches the handler as nothing and
/// raises about OUR internals, naming neither the verb the model omitted nor the
/// agent it addressed (python: `'NoneType' object has no attribute 'get'`).
/// [`crate::tool_parse`]
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
    notice: bool,
) {
    let born = crate::state::incarnation(&AgentId::from(target));
    // ONE CHAIN PER AGENT (#227 Bug 4): the chain token is read when the routing
    // decision is made, not when the delivery later runs.
    let chain =
        if notice || fantastic_kernel::current_origin().is_some_and(|o| o.as_str() == target) {
            state_for(&AgentId::from(target))
                .chain
                .lock()
                .expect("chain poisoned")
                .token
                .clone()
        } else {
            String::new()
        };
    deliver_soon_with_born(
        kernel,
        target,
        text,
        client_id,
        born,
        notice,
        chain,
    );
}

/// NOT AWAITED. A `send` to an AI agent takes that agent's FIFO lock and does
/// not return until the whole turn is done — model call, streaming, and that
/// turn's own routing. Awaiting each delivery inline makes reply 2 of a 4-call
/// turn wait for the entire conversation reply 1 started, and if the chain ends
/// first it is never delivered at all — calls left unanswered, easily misread as
/// a model being stubborn.
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
    born: u64,
    notice: bool,
    chain: String,
) {
    let target_id = AgentId::from(target);
    let work_kernel = Arc::clone(kernel);
    let (target, text) = (target.to_string(), text.to_string());
    let client_id = client_id.to_string();
    kernel.spawn_task(&target_id, "deliver_reply", async move {
        deliver(
            &work_kernel,
            &target,
            &text,
            &client_id,
            born,
            notice,
            chain,
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
/// Woken, the rebooted agent would finish its predecessor's work instead of the
/// new instruction.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn deliver(
    kernel: &Arc<Kernel>,
    target: &str,
    text: &str,
    client_id: &str,
    born: u64,
    notice: bool,
    chain: String,
) {
    let target_agent = AgentId::from(target);
    let late = crate::state::incarnation(&target_agent) != born;
    let mut payload = Map::new();
    payload.insert("type".to_string(), json!("send"));
    payload.insert("text".to_string(), json!(text));
    payload.insert("client_id".to_string(), json!(client_id));
    // ONE CHAIN PER AGENT (#227): a reply routed back to the agent whose chain
    // is running is that chain's own turn — it carries the chain token, so it is
    // never held.
    // Harness self-turns (the held-message wake) are chain turns too.
    // The token captured when the routing decision was made (#227 Bug 4).
    if !chain.is_empty() {
        payload.insert("chain".to_string(), json!(chain));
    }
    // HARNESS TEXT, NOT A MESSAGE: a turn this layer writes to
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
    // Discarding it makes a chain that DIED mid-flight identical to a model that
    // chose to stop: no error anywhere, the last turn simply never happens.
    // Only the first send's error would be visible, i.e. never the ones that
    // matter. This does NOT raise: the routing dispatch is detached, and
    // unwinding here would kill sibling branches that are still fine.
    //
    // THE ANNOUNCEMENT IS AN ORDINARY `status` EVENT — same shape a failed direct
    // send produces (`{type:'status', source, phase:'failed', detail:{…}, ts,
    // client_id}`), because that is the shape reflect's `emits` documents and the
    // shape a UI/harness subscribes to: `reason`/`error` ride in `detail`, where
    // every subscriber reads `ev.detail.reason`.
    // FAILED IS `failed`/`timed_out` — python `out["status"] in ("failed",
    // "timed_out")` — plus any truthy `error` on the reply itself. `cancelled`
    // is NOT announced: an operator kill already told whoever asked for it.
    let reply_error = out.result.get("error").filter(|v| !v.is_null());
    let failed = matches!(out.status, Status::Failed | Status::TimedOut) || reply_error.is_some();
    if failed && notice {
        // ONE CHAIN PER AGENT (#227 Bug 3): a wake that failed ends the chain and
        // releases the held senders (their callers get the cancelled shape).
        state_for(&target_agent)
            .chain
            .lock()
            .expect("chain poisoned")
            .reset("send: the turn that was to read this message did not run");
    }
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
            "failed",
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

/// #634: a `reflect` reply's `view` is how the agent is DRAWN for a person; the
/// model gets its kind and size only. `None` when the reply carries no view body.
/// The store keeps the reply whole.
fn hide_view_body(shown: &Value) -> Option<Value> {
    let view = shown.get("view")?.as_object()?;
    let body = view.get("bytes").or_else(|| view.get("html"))?.as_str()?;
    let kind = view.get("kind").and_then(Value::as_str).unwrap_or("view");
    let mut out = shown.clone();
    out["view"] = json!(format!(
        "{kind}, {} chars, not shown",
        body.chars().count()
    ));
    Some(out)
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

/// An `inquiry` an AI received and has not answered yet — python's
/// `_open_inquiries` value: the asker (`None` for the operator), the pid its reply
/// tag carries, the asker's own thread, and the request id.
pub(crate) struct OpenInquiry {
    pub(crate) asker: Option<String>,
    pub(crate) pid: String,
    pub(crate) reply_client: String,
    pub(crate) request_id: Value,
}

/// OPEN INQUIRIES (fully async), by the receiver's chain
/// `(receiver, client_id)`. The answer the model gives in ANY turn of that chain —
/// or the news that it ended without one — goes to the asker as a later message
/// under the inquiry's pid. Python `_open_inquiries`.
pub(crate) fn open_inquiries() -> &'static Mutex<HashMap<(String, String), OpenInquiry>> {
    static OPEN: OnceLock<Mutex<HashMap<(String, String), OpenInquiry>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The askers of the inquiries `receiver` has received and not yet answered: sorted,
/// each once. The operator (no calling agent) is named `"operator"`, the name
/// `stop` already gives it (`verbs.rs`).
pub(crate) fn inquiry_askers(receiver: &str) -> Vec<String> {
    let mut askers: Vec<String> = open_inquiries()
        .lock()
        .expect("open inquiries poisoned")
        .iter()
        .filter(|((r, _), _)| r == receiver)
        .map(|(_, o)| o.asker.clone().unwrap_or_else(|| "operator".to_string()))
        .collect();
    askers.sort();
    askers.dedup();
    askers
}

/// The jobs an agent has started and not yet heard end, by owner.
type RunningJobs = HashMap<String, Vec<(Value, String)>>;

/// Exec jobs an agent started and has not yet heard end, by owner: `(raw job_id, exec
/// agent)`. An owner with any is awaiting its own work (#306); each one ends in ONE
/// `job_done` wake, after which it is dropped from here.
pub(crate) fn running_jobs() -> &'static Mutex<RunningJobs> {
    static JOBS: OnceLock<Mutex<RunningJobs>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drops one job from [`running_jobs`] when it goes out of scope, so a watcher that is
/// cancelled (its agent stopped or deleted) stops counting the job as pending.
struct RunningJob {
    owner: String,
    job: Value,
    exec: String,
}

impl Drop for RunningJob {
    fn drop(&mut self) {
        if let Ok(mut jobs) = running_jobs().lock() {
            let empty = match jobs.get_mut(&self.owner) {
                Some(list) => {
                    list.retain(|(j, e)| !(j == &self.job && e == &self.exec));
                    list.is_empty()
                }
                None => false,
            };
            if empty {
                jobs.remove(&self.owner);
            }
        }
    }
}

/// A field of the job a `status` or `tail` reply is about: the first of `jobs`, or the
/// reply itself when the exec agent answers flat.
fn job_field<'a>(result: &'a Value, key: &str) -> Option<&'a Value> {
    result
        .get("jobs")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|j| j.get(key))
        .or_else(|| result.get(key))
}

/// Record a job an agent's `exec start` left running, and spawn ONE watcher task for it
/// on the owner. The watcher waits on the exec agent's `status` until the job is no
/// longer live (`running` or `stopping`), forgets the job, then wakes the owner once with
/// `job_done {job_id, exit_code, lines, tail}`: `tail` is the job's last 20 lines and
/// `lines` the count of every line it printed (the exec bridge's `printed`), so a tail
/// shorter than `lines` says the output was cut. The exit key is `exit_code`, as the
/// exec bridge names it, not `exit`.
pub(crate) fn watch_job(
    kernel: &Arc<Kernel>,
    owner: &str,
    exec: &str,
    job_id: &Value,
    client_id: &str,
) {
    running_jobs()
        .lock()
        .expect("jobs poisoned")
        .entry(owner.to_string())
        .or_default()
        .push((job_id.clone(), exec.to_string()));
    let work = Arc::clone(kernel);
    let (owner_s, exec_s, job, cid) = (
        owner.to_string(),
        exec.to_string(),
        job_id.clone(),
        client_id.to_string(),
    );
    kernel.spawn_task(&AgentId::from(owner), "job_watch", async move {
        let entry = RunningJob {
            owner: owner_s.clone(),
            job: job.clone(),
            exec: exec_s.clone(),
        };
        let execution_id = AgentId::from(exec_s.as_str());
        let mut last;
        loop {
            let out = work
                .send(vec![Item::new(
                    execution_id.clone(),
                    json!({"type": "status", "job_id": job.clone(), "wait_s": 100}),
                )])
                .await;
            last = out.first().map_or(Value::Null, |o| o.result.clone());
            let state = job_field(&last, "status")
                .and_then(Value::as_str)
                .unwrap_or("");
            // An error or unknown-job reply has no status, so it ends the wait too.
            if state != "running" && state != "stopping" {
                break;
            }
        }
        let t = work
            .send(vec![Item::new(
                execution_id,
                json!({"type": "tail", "job_id": job.clone(), "lines": 20}),
            )])
            .await;
        // every line the job printed, of which `tail` shows the last 20
        let lines = t
            .first()
            .and_then(|o| o.result.get("printed"))
            .cloned()
            .unwrap_or(Value::Null);
        let tail = t
            .first()
            .and_then(|o| o.result.get("lines"))
            .and_then(Value::as_array)
            .map(|ls| {
                let skip = ls.len().saturating_sub(20);
                ls[skip..]
                    .iter()
                    .filter_map(|l| l.get("line").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let exit = job_field(&last, "exit_code")
            .cloned()
            .unwrap_or(Value::Null);
        // Forget the job BEFORE the wake, so the woken turn's `pending` no longer counts it.
        drop(entry);
        let text = format!(
            "job_done {}",
            json!({"job_id": job, "exit_code": exit, "lines": lines, "tail": tail})
        );
        deliver_soon(&work, &owner_s, &text, &cid, false);
    });
}

/// Close the inquiry this chain `(self_id, client_id)` is answering: hand `body`
/// (`{answer}` or `{error}`) to its asker as `[reply to <pid> · <self_id> inquiry]`.
/// The answer is THIS agent's own send to the asker, gated like
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
    let err = fantastic_kernel::refusal::error_text(&out.result);
    if err.is_some() || matches!(out.status, Status::Failed | Status::TimedOut) {
        let why = err.unwrap_or_else(|| out.status.as_str().to_string());
        deliver_soon(
            kernel,
            self_id,
            &format!("[your inquiry answer did not reach {asker}: {why}]"),
            client_id,
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
    // verbatim (e.g. "..." written as the verdict).
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

/// The model's `keep`: `<of>` the id a reply block names, `<summary>` what to keep of
/// it. `Some` is the error block for the model.
async fn keep_note(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    call: &crate::tool_parse::ToolCall,
) -> Option<String> {
    let of = call
        .arguments
        .get("of")
        .and_then(|v| crate::tool_parse::typed::integer("of", v).ok());
    let summary = call.arguments.get("summary").and_then(Value::as_str);
    let (Some(of), Some(summary)) = (of, summary) else {
        return Some(
            "keep: it takes `<of>` the id of the reply and `<summary>` what to keep of it"
                .to_string(),
        );
    };
    crate::memory::save_keep(self_id, kernel, of, summary)
        .await
        .err()
}

/// Drop a deleted agent's jobs and open inquiries
/// ([`crate::verbs::on_delete`]).
pub(crate) fn forget(self_id: &str) {
    running_jobs()
        .lock()
        .expect("jobs poisoned")
        .remove(self_id);
    open_inquiries()
        .lock()
        .expect("open inquiries poisoned")
        .retain(|chain, _| chain.0 != self_id);
}

/// Honour where this pass's output goes — the DETACHED routing task. The
/// model's calls are the agent's calls: the task is one the agent's own
/// dispatch spawned (`Kernel::spawn_task`), so every send in it is the agent's
/// and is gated as the agent.
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
/// DISPATCHES, so the pass sends nothing, its closed calls included.
#[allow(clippy::too_many_arguments)]
pub async fn route_reply(
    self_id: AgentId,
    kernel: Arc<Kernel>,
    text: String,
    client_id: String,
    send_to: Option<String>,
    send_timeout: f64,
    model_window: i64,
    message_id: Option<i64>,
    call_streams: Vec<(String, String)>,
    cut: bool,
) {
    // THE MODEL'S CALLS ARE THE AGENT'S CALLS — gated. The routing task is
    // spawned by the agent's own dispatch through `Kernel::spawn_task`, which
    // carries the dispatch identity into it (a bare tokio spawn would not):
    // dispatches, the binary carrier, and reply deliveries all attribute to the
    // AI agent.
    let (wake_kernel, wake_id) = (Arc::clone(&kernel), self_id.clone());
    let goes_on = route_reply_gated(
        self_id,
        kernel,
        text,
        client_id,
        send_to,
        send_timeout,
        model_window,
        message_id,
        call_streams,
        cut,
    )
    .await;
    if !goes_on {
        chain_ended(&wake_kernel, &wake_id);
    }
}

/// The route's default deadline for a call to an agent, in seconds. PRECEDENCE:
/// the call's own `timeout` (applied where the item is built) > the root
/// record's `send_timeout` (any finite positive number, no floor) > the env
/// `FANTASTIC_AI_SEND_TIMEOUT` > 600 ([`crate::constants::send_timeout_secs`]).
/// Only the route default reads the root field: the generation and held-wait
/// ceilings keep `send_timeout_secs()`.
pub(crate) fn route_deadline(kernel: &Kernel) -> f64 {
    kernel
        .root()
        .and_then(|r| r.record().meta.get("send_timeout").cloned())
        .and_then(|v| v.as_f64())
        .unwrap_or_else(crate::constants::send_timeout_secs)
}

/// ONE CHAIN PER AGENT (#227): the chain has ended. Messages held while it ran
/// are answered by ONE wake turn; with none held, the chain is no longer alive.
///
/// The wake can be lost (refused by the gate, a send error, an agent being shut
/// down), so a watchdog releases the senders it was for if no pass has taken them
/// within the send ceiling.
pub(crate) fn chain_ended(kernel: &Arc<Kernel>, self_id: &AgentId) {
    chain_ended_within(
        kernel,
        self_id,
        std::time::Duration::from_secs_f64(crate::constants::send_timeout_secs()),
    );
}

/// [`chain_ended`] with the watchdog's window given.
pub(crate) fn chain_ended_within(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    ceiling: std::time::Duration,
) {
    let state = state_for(self_id);
    let (held, ids) = {
        let mut chain = state.chain.lock().expect("chain poisoned");
        // The held senders STAY in `chain.held`: the wake pass takes and answers them.
        let held: Vec<String> = chain.held.iter().map(|h| h.client_id.clone()).collect();
        let ids: Vec<String> = chain.held.iter().map(|h| h.send_id.clone()).collect();
        if held.is_empty() {
            chain.alive = false;
        }
        (held, ids)
    };
    if held.is_empty() {
        return;
    }
    let text = format!(
        "[{} message(s) arrived while you were working — they are above. Answer them.]",
        held.len()
    );
    deliver_soon(
        kernel,
        self_id.as_str(),
        &text,
        &held[0],
        true,
    );
    let watched = self_id.clone();
    let gone = state.chain.lock().expect("chain poisoned").held_gone.clone();
    kernel.spawn_task(self_id, "chain_wake_watch", async move {
        // The watchdog ends as soon as the senders it watches are no longer held
        // (the wake pass took them); on the ceiling it releases them.
        let deadline = tokio::time::Instant::now() + ceiling;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                _ = gone.notified() => {
                    let st = state_for(&watched);
                    let chain = st.chain.lock().expect("chain poisoned");
                    if !chain.held.iter().any(|h| ids.contains(&h.send_id)) {
                        return;
                    }
                }
            }
        }
        state_for(&watched)
            .chain
            .lock()
            .expect("chain poisoned")
            .expire(
                &ids,
                "send: no turn read this message within the send ceiling",
            );
    });
}

/// A CHAIN CUT BY A RESTART (#726). Everything is records: at its start the kernel
/// settled the calls the previous process left in flight as dead letters
/// ([`fantastic_kernel::executions::CutCall`]); the ones this agent's own tasks made
/// and the messages sent to it reach it here, at its boot, as ONE notice turn of a
/// new chain. A reply it was waiting for will not come (resend or drop it); a message
/// it held or was answering was cut, and its sender is told by its own dead letter.
/// Never silent. A call made inside a dispatch (a pass's own store or provider call)
/// is the pass's plumbing, not the agent's work: the cut message that ran the pass
/// says it. Nothing cut, nothing told.
pub(crate) fn resume_cut(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    cut: &[fantastic_kernel::executions::CutCall],
) {
    let me = self_id.as_str();
    let lines: Vec<String> = cut
        .iter()
        .filter_map(|c| {
            let made = c.caller.as_deref() == Some(me) && c.task.is_some();
            let got = c.target == me && c.verb.as_deref() == Some("send");
            if made && c.target == me {
                Some(format!(
                    "[your own turn \u{b7} dead letter {}] a reply or reminder on its way to \
                     you was cut before you read it",
                    c.id
                ))
            } else if made {
                Some(format!(
                    "[reply to {} \u{b7} {} {}] no reply will come \u{2192} dead letter {}; \
                     resend or drop",
                    c.id,
                    c.target,
                    c.verb.as_deref().unwrap_or("?"),
                    c.id
                ))
            } else if got && c.caller.as_deref() != Some(me) {
                Some(format!(
                    "[from {} \u{b7} dead letter {}] a message to you was cut before it was \
                     answered; its sender is told \u{2014} do not answer it",
                    c.caller.as_deref().unwrap_or("operator"),
                    c.id
                ))
            } else {
                None
            }
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    let text = format!(
        "[restart] The kernel stopped while this work was in flight, and a new kernel \
         started on the same records. What was in flight cannot finish; each is a dead \
         letter now:\n{}\nGo on from here.",
        lines.join("\n")
    );
    deliver_soon(
        kernel,
        me,
        &text,
        &fantastic_kernel::inquiry::default_client_id(),
        true,
    );
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
    send_timeout: f64,
    model_window: i64,
    message_id: Option<i64>,
    call_streams: Vec<(String, String)>,
    cut: bool,
) -> bool {
    // ONE CHAIN PER AGENT (#227): true while the chain goes on (see the caller).
    let mut goes_on = false;
    // THE MATCH: a dispatched message carries the `stream_id`
    // of the part it was streamed as, so a consumer settles its draft on it.
    // Matched by the call's exact body, in order — one reading of the text, not
    // two.
    let mut streams = call_streams;
    // ONLY THE ANSWER ACTS: calls written inside thinking are
    // read as thinking — dispatch reads the answer.
    // A malformed or foreign call never reaches this point: the turn re-asked its model
    // inside the attempt loop (#766), so the answer routed is the accepted one.
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

    let done_called = calls.iter().any(|c| c.name == "done");
    // `stop` (#766, 1.8): the model says the work cannot be done, and why. The reason of
    // its first `stop` that has one; the reader refused a `stop` without one (S25).
    let stop_called: Option<String> = calls.iter().filter(|c| c.name == "stop").find_map(|c| {
        c.arguments
            .get("reason")
            .and_then(Value::as_str)
            .filter(|r| !r.trim().is_empty())
            .map(str::to_string)
    });
    // `keep` and `line_summary` are notes, not actions: a turn of notes alone is a turn
    // with no call (#641, #709); the attempts re-asked it (#795), so only a cut pass
    // routes with none
    let any_calls = calls.iter().any(acting);
    // ONE TURN, ONE BATCH. Every call the model emitted is dispatched in a
    // SINGLE `kernel.send([...])` — concurrent, side-effect order across
    // calls undefined — and everything owed to one destination arrives as
    // ONE delivered turn.
    //
    // Why whole, never split: replies landing one per turn make a turn that
    // fanned out 7 calls spend the next 7 turns collecting its own answers,
    // and every question asked in between is answered turns later by
    // something older — the model re-asks and the run builds nothing.
    // Anthropic documents the split shape as the anti-pattern
    // ("separate user messages reduce parallel tool use"), OpenAI 400s on a
    // missing tool result, LangGraph's ToolNode collects every ToolMessage
    // before the agent runs again. The model never sees a PARTIAL batch, so
    // it never re-asks for the missing half.
    let mut items: Vec<Item> = Vec::new();
    // The deadline each item runs under, as the model is told it (parallel to
    // `items`): the call's own `timeout` as given, else the route default.
    let mut deadlines: Vec<String> = Vec::new();
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

    // WHAT THE MODEL KEEPS of a reply that will fold (#641): stored for the fold; a
    // reply that is not in the conversation is refused, saying so.
    for call in calls.iter().filter(|c| c.name == "keep") {
        if let Some(err) = keep_note(&kernel, &self_id, call).await {
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

    // WHAT THE CHAT DOES NOW (#709): its current line, kept and published; a chat that
    // does not offer it, or a call without a line, is told so.
    for call in calls.iter().filter(|c| c.name == crate::line_summary::CALL) {
        if let Some(err) = crate::line_summary::take(&kernel, &self_id, call).await {
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

    for call in calls {
        if CONTROL_CALLS.contains(&call.name.as_str()) {
            // CONTROL CALLS, not dispatches. `done` ends the answer: nothing more
            // now, the next message wakes the agent; `inquiry_answer` hands this
            // turn's answer back to whoever sent the inquiry (the `inquiry` verb
            // reads it); `stop` ends the chain as failed. None is a send, none is an
            // error.
            continue;
        }
        let args = &call.arguments;
        let chosen: Option<&Value> = args.get("target_id").or_else(|| args.get("target"));
        let target: Option<&str> = chosen
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // `reply_to` DEFAULTS TO SELF. An omitted key means the model gets its
        // own answer back, not that the answer is thrown away.
        //
        // A fire-and-forget default makes the one failure a model can never
        // debug: a key left out of an otherwise perfect call runs, its result
        // is dropped, and the chain dies with no reply, no error, and nothing
        // to see. From inside the model that is indistinguishable from being
        // finished.
        //
        // Stopping is still the default and still costs nothing — it is
        // emitting NO call. A dropped key costs a wasted turn instead of
        // the whole task, and discarding a reply is something you must SAY
        // (`reply_to: null` / "none"), which is the right way round.
        // The tag grammar reads text (`<reply_to>1001</reply_to>` is "1001"): a
        // number is an id the model wrote, never "discard" — it is checked like
        // any id below.
        let reply_to: Option<String> = match args.get("reply_to") {
            Some(v) => match v {
                Value::String(s) => Some(s.clone()),
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
        // answer goes, never an argument the target sees; one naming no agent would
        // fail the delivery on that missing agent's inbox, so nobody sees it and the
        // model never learns what its call did. Nothing is dispatched; the model is
        // told. A peer address (`<guid>|<agent>`) is the peer's to resolve.
        if let Some(rt) = reply_to.as_deref() {
            if rt != self_id.as_str() && !rt.contains('|') && kernel.resolve_target(rt).is_none() {
                let verb = match args.get("payload") {
                    Some(Value::Object(p)) => p.get("type").map_or("?".to_string(), crate::helpers::shown),
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
        // turn it is — without this, a model-issued `grep` lands on the default
        // client and returns `total=0` against a store full of matches.
        let declares = |payload: &Map<String, Value>, field: &str| {
            payload
                .get("type")
                .and_then(Value::as_str)
                .and_then(|verb| {
                    fantastic_kernel::typing::verb_schema(&kernel, target, verb, &Map::new())
                })
                .is_some_and(|schema| schema["properties"].get(field).is_some())
        };
        if target == self_id.as_str()
            && !payload.contains_key("client_id")
            && declares(&payload, "client_id")
        {
            payload.insert("client_id".to_string(), json!(client_id));
        }
        // THE PARSER READ TEXT: each value takes the type the target verb's schema
        // states (a `limit` of `7` is the integer 7, a `path` of `007` stays text).
        if let Err(refused) = crate::tool_parse::typed::payload(&kernel, target, &mut payload).await {
            if let Some(rt) = reply_to {
                units.push(Unit {
                    dest: rt,
                    target: target.to_string(),
                    verb: "-".to_string(),
                    idx: None,
                    body: Some(crate::tool_parse::render_reply(&refused)),
                    title: None,
                });
            }
            continue;
        }
        // The call's own bounds are read as the numbers they are, or the call is refused.
        let (timeout_read, retries_read) = match (
            args.get("timeout")
                .map(|t| crate::tool_parse::typed::routing("timeout", t))
                .transpose(),
            args.get("retries")
                .map(|r| crate::tool_parse::typed::routing("retries", r))
                .transpose(),
        ) {
            (Ok(t), Ok(r)) => (t, r),
            (Err(refused), _) | (_, Err(refused)) => {
                if let Some(rt) = reply_to {
                    units.push(Unit {
                        dest: rt,
                        target: target.to_string(),
                        verb: "-".to_string(),
                        idx: None,
                        body: Some(crate::tool_parse::render_reply(&refused)),
                        title: None,
                    });
                }
                continue;
            }
        };
        // python `payload.get("type", "?")`, then `str()` into the reply tag: a
        // NON-STRING type tags the reply with its own text (`… a 42 …`,
        // `… a True …`, `… a {'a': 'x'} …`), and `?` means the key was ABSENT.
        // `as_str().unwrap_or("?")` printed `?` for every one of them.
        let verb = match payload.get("type") {
            Some(v) => crate::helpers::shown(v),
            None => "?".to_string(),
        };
        // THE `bytes` ARGUMENT IS RAW BYTES, NOT A JSON STRING.
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
        if declares(&payload, "reply_client_id") {
            payload.insert("reply_client_id".to_string(), json!(client_id));
        }
        payloads.push((target.to_string(), Value::Object(payload.clone())));
        let mut item = Item::new(AgentId::from(target), Value::Object(payload.clone()));
        if let Some(doc) = document {
            item = item.with_binary_key("bytes", doc);
        }
        // EVERY dispatched call carries the layer's deadline. The KERNEL
        // defaults nothing, so without it a call to a handler that never returns
        // hangs the whole batch forever here — no reply, no timeout block, the
        // chain simply stops. Set BEFORE the model's own `timeout` arg, which
        // overrides it; the kernel validates that value, so a bad one settles
        // as a refusal the model can read.
        item = item.with_timeout(send_timeout);
        // The model may NAME the execution (dedup/join across retries) and may
        // bound its own call; each is carried in its declared type. A verb's own `id`
        // (`delete_agent`, ...) is in `<payload>` and reaches the target.
        if let Some(id) = args.get("id") {
            match serde_json::from_value::<fantastic_kernel::declared::IdOrName>(id.clone()) {
                Ok(id) => item = item.with_id(id),
                Err(_) => {
                    let refused = fantastic_kernel::refusal::bad("send", "id", "text or an integer");
                    if let Some(rt) = reply_to {
                        units.push(Unit {
                            dest: rt,
                            target: target.to_string(),
                            verb: "-".to_string(),
                            idx: None,
                            body: Some(crate::tool_parse::render_reply(&refused)),
                            title: None,
                        });
                    }
                    continue;
                }
            }
        }
        if let Some(t) = timeout_read.as_ref().and_then(Value::as_f64) {
            item = item.with_timeout(t);
        }
        if let Some(r) = retries_read.as_ref().and_then(Value::as_u64) {
            item = item.with_retries(u32::try_from(r).unwrap_or(u32::MAX));
        }
        items.push(item);
        deadlines.push(
            timeout_read
                .as_ref()
                .map(crate::helpers::shown)
                .unwrap_or_else(|| fantastic_bundle::py_float_repr(send_timeout)),
        );
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

    let outcomes = if items.is_empty() {
        Vec::new()
    } else {
        kernel.send(items).await
    };

    // A JOB this agent started (#306): an `exec start` answered `running` is watched, and
    // its end wakes this agent once. Only this batch's own replies are read, so a job
    // another agent started never registers against this one.
    for ((exec, sent), out) in payloads.iter().zip(outcomes.iter()) {
        let started = sent.get("type").and_then(Value::as_str) == Some("start")
            && out.result.get("status").and_then(Value::as_str) == Some("running");
        match out.result.get("job_id").filter(|v| !v.is_null()) {
            Some(job) if started => watch_job(&kernel, self_id.as_str(), exec, job, &client_id),
            _ => {}
        }
    }

    // The result — INCLUDING an error — goes back. A call that failed silently
    // is the one thing a model can never debug.
    //
    // PID first, then target+verb. Target+verb alone is NOT unique: with two
    // calls in flight to one agent `[reply to kernel_state set]` is unmatchable
    // — the model re-issues calls and blames "duplicate delivery" or a reply
    // queue "running ahead", then re-polls forever.
    // The PID is the same id `ps`/`cancel` use, so a reply names a row the
    // model can act on.
    let meta = crate::helpers::agent_meta(&self_id, &kernel);
    let cap = crate::context::inline_cap(&meta, model_window);
    // THE CAP IS PER MESSAGE, not per reply: every reply of a
    // batch lands in ONE message to its addressee, so the replies share the cap.
    // A reply that would push its batch past it is held like an oversized one:
    // two replies each under the cap can together overflow a small window
    // (`too_small`).
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
                    let mut shown = reply_as_text(out);
                    // ANOTHER AGENT'S THINKING NEVER LANDS HERE: a peer's rows
                    // (`recall`, `rows`) are read without it. This agent's own
                    // read of its own rows keeps it.
                    if u.target != self_id.as_str() {
                        if let Some(clean) = crate::think::without_thinking_fields(&shown) {
                            shown = std::borrow::Cow::Owned(clean);
                        }
                    }
                    if u.verb == "reflect" {
                        if let Some(trimmed) = hide_view_body(&shown) {
                            shown = std::borrow::Cow::Owned(trimmed);
                        }
                    }
                    // A `send` that passed its deadline went to the dead letter:
                    // the sender is told where, and how to resend or drop it.
                    let dead_letter = u.verb == "send"
                        && out.status == Status::TimedOut
                        && out.pid.is_some();
                    if dead_letter {
                        if let Some(obj) = shown.to_mut().as_object_mut() {
                            obj.insert(
                                "error".to_string(),
                                json!(format!(
                                    "your message to {} hit its {}s deadline \u{2192} dead letter {pid}; resend or drop",
                                    u.target, deadlines[i]
                                )),
                            );
                        }
                    }
                    let pointed = point_to_docs(&u.target, &u.verb, &shown);
                    let mut body =
                        crate::tool_parse::render_reply(pointed.as_ref().unwrap_or(&shown));
                    // FULL ASYNC. A reply is DATA, not a question — the model
                    // needs to be able to SEE it, not be forced to swallow it
                    // whole as its next turn. Anything over the inline cap is
                    // held in RAM under its pid and replaced by its digest + its
                    // ORIGIN line. SYNC OR ASYNC ALIKE: a
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
                    if u.verb == "send" && out.pid.is_some() && !dead_letter {
                        body = format!("(deadline {}s)\n{body}", deadlines[i]);
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

    // THE TAIL. An answer reaching here has calls (the attempts re-asked one with none,
    // #795), or it was cut and dispatched nothing. Nothing reminds and nothing counts
    // streaks: the answer ends with `done` or `stop`, and the next message wakes the
    // agent.
    let any_blocks = !blocks.is_empty();
    // The chain goes on only when a reply batch lands on this agent itself.
    let to_self = blocks.iter().any(|b| b.dest == self_id.as_str());
    if any_blocks {
        deliver_grouped(
            &kernel,
            self_id.as_str(),
            blocks,
            &client_id,
            message_id,
        )
        .await;
        goes_on = to_self;
    }
    if let Some(reason) = stop_called.as_deref() {
        // THE MODEL STOPPED (#766, 1.8): the work cannot be done. The chain ends as
        // failed through the same give-up as the harness's stop: status `failed`,
        // `reason: gave_up`, the model's reason as `error`, the open inquiry closed with
        // it, `send_to` told. It wins over `done` in one answer; the calls written beside
        // it were dispatched above, as beside `done` (section 5, item 5).
        give_up(&kernel, &self_id, &client_id, send_to.as_deref(), reason).await;
        return false;
    }
    if done_called {
        // Nothing more from this agent now: the next message wakes it, an answer owed
        // to an inquiry it received included (#795, Q5). A caller's `send_to` hand-off
        // is the one thing that still applies: the finished text is what it asked to
        // have delivered.
        if !any_blocks {
            if let Some(to) = send_to.as_deref() {
                deliver_soon(&kernel, to, &text, &client_id, false);
            }
        }
        return false;
    }
    if any_calls {
        // Calls that reply elsewhere bring no message back to this agent, so the
        // chain ends here unless a reply batch landed on self (#227 Bug 1).
        return to_self;
    }
    // No call of the model's own (a cut pass), so the CALLER's one-shot handoff
    // applies: hand this pass's TEXT to `send_to`, once. It must not propagate — a
    // caller cannot spend the model's turns for it.
    if let Some(to) = send_to.as_deref() {
        deliver_soon(&kernel, to, &text, &client_id, false);
    }
    goes_on
}

/// A call that acts: anything but the notes `keep` and `line_summary` (#641, #709).
fn acting(call: &crate::tool_parse::ToolCall) -> bool {
    call.name != "keep" && call.name != crate::line_summary::CALL
}

/// Whether an answer (its internal text, thinking marked) acts: one terminated call in
/// its ANSWER that is not a note. The attempts re-ask one that does not (#795).
pub(crate) fn acts(text: &str) -> bool {
    let answer = crate::call_check::repair(&crate::think::without_thinking(text));
    terminated_calls(&answer).iter().any(|(c, _)| acting(c))
}

/// The calls that are not dispatches: they end or answer the chain, (`keep`)
/// note a reply for its fold, (`line_summary`) say in one line what the chat does now, or
/// (`stop`) end the chain as failed with a reason.
const CONTROL_CALLS: [&str; 5] = ["done", "inquiry_answer", "keep", "line_summary", "stop"];

/// The give-up tail: say so in the status, close the inquiry and tell the caller.
pub(crate) async fn give_up(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    client_id: &str,
    send_to: Option<&str>,
    gave_up: &str,
) {
    let st = state_for(self_id);
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("gave_up"));
    detail.insert("error".to_string(), json!(gave_up));
    emit_status(kernel, &st, self_id, client_id, "failed", detail).await;
    let mut body = Map::new();
    body.insert("error".to_string(), json!(gave_up));
    close_inquiry(kernel, self_id.as_str(), client_id, body).await;
    if let Some(to) = send_to {
        deliver_soon(kernel, to, gave_up, client_id, false);
    }
}

/// ONE delivered message per destination: blocks joined by a blank line, each
/// tagged `[reply to <pid> · <target> <verb>]` — with ` · N of M in batch <token>`
/// composed INSIDE that tag when the destination is owed more than one.
///
/// INSIDE, not a trailing line: that is what the shared prompt teaches and what
/// python's history trimmers can read (`_REPLY_TAG` / `_BLOCK_SPLIT` match the
/// tag at the START of a block), so transcripts trim and byte-diff alike across
/// runtimes.
///
/// The stamp is for CORRELATION and COMPLETENESS, not "the rest are still in
/// flight" — the batch arrives whole. A model that made 2 calls and got 1 back
/// cannot tell "still running" from "answered the wrong one".
async fn deliver_grouped(
    kernel: &Arc<Kernel>,
    self_id: &str,
    blocks: Vec<Block>,
    client_id: &str,
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
            born.get(&dest).copied().unwrap_or_default(),
            notice,
            // ONE CHAIN PER AGENT (#227 Bug 4): the token is read when this routing
            // decision is made, as in `deliver_soon`.
            if notice
                || fantastic_kernel::current_origin().is_some_and(|o| o.as_str() == dest.as_str())
            {
                state_for(&AgentId::from(dest.as_str()))
                    .chain
                    .lock()
                    .expect("chain poisoned")
                    .token
                    .clone()
            } else {
                String::new()
            },
        );
    }
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
    fn a_reflect_view_body_is_hidden_from_the_model_but_kind_and_size_stay() {
        let r = json!({"id": "c", "view": {"kind": "html", "bytes": "<p>héllo</p>", "calls": {}}});
        let shown = hide_view_body(&r).expect("a view body is hidden");
        assert_eq!(shown["view"], json!("html, 12 chars, not shown"));
        assert_eq!(shown["id"], "c");
        assert_eq!(r["view"]["bytes"], "<p>héllo</p>", "the original is whole");
        assert!(hide_view_body(&json!({"id": "c"})).is_none());
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
             it goes to. Shape: <call name=\"send\"><routing><target_id>agent</target_id>\
             </routing><payload><type>verb</type></payload></call>. Send `kernel` <type>reflect</type> for the \
             list of agent ids."
        );
        assert_eq!(
            no_payload_error("totals"),
            "send to 'totals' carried no payload, so nothing was dispatched. A call \
             names its verb in `<type>`, e.g. <type>reflect</type>."
        );
        // The third is composed by the parser, which OWNS the wording (see
        // `MalformedCall::error` and the literal pin in
        // `tool_parse::tests_malformed`); what this asserts is that the router
        // delivers THAT, verbatim, with nothing reworded on the way through.
        let bad =
            crate::tool_parse::malformed_calls(
                "<call name=\"send\"><routing><reply_to>self</reply_to></routing>\
                 <payload><type>reflect</type></payload></call>",
            );
        assert_eq!(bad.len(), 1, "one opened, unparseable envelope: {bad:?}");
        assert!(
            bad[0]
                .error
                .starts_with("call NOT dispatched: a send names its target"),
            "verbatim prefix: {:?}",
            bad[0].error
        );
        assert!(
            bad[0].error.ends_with(&format!(
                ". Nothing ran. {}",
                crate::model_format::default::HINT
            )),
            "verbatim hint: {:?}",
            bad[0].error
        );
        assert!(
            bad[0].body().starts_with(&bad[0].error),
            "the body the router delivers opens with that exact sentence"
        );
    }

    /// `target` is accepted beside `target_id`: an absent `target_id` yields to
    /// `target`, a present one of any shape does not (a non-string one earns the
    /// shape error instead of being silently re-read).
    #[test]
    fn target_fallthrough_is_absence_only() {
        let f = |v: Value| {
            let obj = v.as_object().unwrap().clone();
            let chosen: Option<&Value> = obj.get("target_id").or_else(|| obj.get("target"));
            chosen
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        assert_eq!(f(json!({"target_id": "a"})), Some("a".to_string()));
        assert_eq!(f(json!({"target": " b "})), Some("b".to_string()));
        assert_eq!(f(json!({"target_id": "", "target": "b"})), None);
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
