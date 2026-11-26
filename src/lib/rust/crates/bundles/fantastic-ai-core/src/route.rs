//! Reply routing — where one pass's answer goes (#795).
//!
//! The model's own `<call>`s: each names a target, a payload and `reply_to`. Each goes
//! out through [`Kernel::call`] — the ack at once, the settle later — and each reply is
//! delivered, when it comes, as ONE message to its own `reply_to` (default: the agent
//! itself), tagged `[reply to <pid> · <target> <verb> · ↑<answer> · k of M in batch
//! <b>]`. Replies are never held until a batch is whole: each one wakes the agent, and
//! the ones that land while a pass runs are read by its next pass. An ack of a message
//! sent to another AI is stored and wakes nothing; a refusal wakes.
//!
//! Nothing here bounds how far the model goes, and nothing waits: every call is a `ps`
//! row and `cancel` kills it, so a runaway is visible and killable exactly like a script
//! agent's `while(true)`.
//!
//! The `_call_stack` recursion guard is NOT propagated from here: a call the model
//! makes to itself would trip [`fantastic_kernel::call_guard`] as a false cycle. The
//! INBOUND check lives in [`crate::verbs::send`].

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

/// HAND `text` TO `dest` as a message. `born` is `dest`'s incarnation (and, to the agent
/// itself, how often its work was stopped) when the routing decided it: a reply that
/// crosses its next boot answers a call the previous incarnation made, and one that
/// crosses a `stop` answers work the stop ended; each is kept as a `late` row that
/// wakes nothing.
///
/// To the agent itself the message is an arrival ([`crate::life::arrive`]); `wakes`
/// false stores it only (an ack, Q2). To another agent it is a `send` through
/// [`Kernel::call`]: an AI acks once the row is stored, and a refused hand-off is said
/// on `dest`'s inbox as a `failed` status, never dropped.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn deliver(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    dest: &str,
    text: &str,
    client_id: &str,
    born: Born,
    notice: bool,
    wakes: bool,
) {
    let target_agent = AgentId::from(dest);
    let late = crate::state::incarnation(&target_agent) != born.0;
    if dest == self_id.as_str() {
        if late || born.1 != crate::life::stops(self_id) {
            if let Err(e) = crate::verbs::save_late(self_id, kernel, text).await {
                eprintln!("  [ai] {self_id}: a late reply could not be kept: {e}");
            }
            return;
        }
        let m = crate::life::Incoming {
            text,
            client_id,
            origin: self_id.as_str(),
            notice,
            attached: None,
        };
        if let Err(e) = crate::life::arrive(kernel, self_id, &m, wakes).await {
            eprintln!("  [ai] {self_id}: a reply could not be stored: {}", e.0);
        }
        return;
    }
    let mut payload = Map::new();
    payload.insert("type".to_string(), json!("send"));
    payload.insert("text".to_string(), json!(text));
    payload.insert("client_id".to_string(), json!(client_id));
    if late {
        // A LATE REPLY TO A PREVIOUS INCARNATION: the TARGET keeps it as a row (a store
        // keys every row by its writer), so it rides a `send` marked `late`.
        payload.insert("late".to_string(), json!(true));
    }
    // owed until the destination took it: its own arrival is counted from then
    crate::life::owe(self_id, 1);
    let (k, d, c, me) = (Arc::clone(kernel), dest.to_string(), client_id.to_string(), self_id.clone());
    let out = kernel
        .call(
            Item::new(AgentId::from(dest), Value::Object(payload)),
            move |out| async move {
                announce_failed(&k, &d, &c, &out).await;
                crate::life::owe(&me, -1);
            },
        )
        .await;
    if out.status != Status::Accepted {
        announce_failed(kernel, dest, client_id, &out).await;
        crate::life::owe(self_id, -1);
    }
}

/// A destination's incarnation and how often its work was stopped, read when the
/// routing decided a delivery.
#[derive(Clone, Copy, Default)]
pub(crate) struct Born(u64, u64);

impl Born {
    /// `dest`'s, now.
    pub(crate) fn of(dest: &str) -> Self {
        let id = AgentId::from(dest);
        Self(crate::state::incarnation(&id), crate::life::stops(&id))
    }
}

/// ANNOUNCE A FAILED HANDOFF on the destination's inbox — the ordinary `status` event a
/// failed send produces (`detail.reason`/`detail.error`). `failed`/`timed_out` or any
/// truthy `error`; `cancelled` is not announced: an operator kill already told whoever
/// asked for it.
async fn announce_failed(kernel: &Arc<Kernel>, dest: &str, client_id: &str, out: &fantastic_kernel::Outcome) {
    let reply_error = out.result.get("error").filter(|v| !v.is_null());
    let failed = matches!(out.status, Status::Failed | Status::TimedOut) || reply_error.is_some();
    if !failed {
        return;
    }
    let why = reply_error.map_or_else(|| json!(out.status.as_str()), |e| e.clone());
    let target_id = AgentId::from(dest);
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("error"));
    detail.insert("error".to_string(), why);
    emit_status(kernel, &state_for(&target_id), &target_id, client_id, "failed", detail).await;
}

/// Whether a stored reply text is the ACK of a message sent to an AI agent
/// (`[reply to N · <agent> send …]` carrying `accepted`): stored, never woken on (Q2).
#[must_use]
pub(crate) fn is_ack_text(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("[reply to ") else {
        return false;
    };
    let (tag, body) = rest.split_once('\n').unwrap_or((rest, ""));
    let verb = tag.split(" \u{b7} ").nth(1).and_then(|tv| tv.split_whitespace().nth(1));
    let verb = verb.map(|v| v.trim_end_matches(']'));
    verb == Some("send") && body.contains("<accepted>true</accepted>") && !body.contains("<error>")
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
    let born = Born::of(owner);
    let (owner_s, exec_s, job, cid) = (
        owner.to_string(),
        exec.to_string(),
        job_id.clone(),
        client_id.to_string(),
    );
    let owing = crate::life::Owing::new(&AgentId::from(owner));
    kernel.spawn_task(&AgentId::from(owner), "job_watch", async move {
        let _owing = owing;
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
        // Forget the job BEFORE the wake, so the woken pass no longer counts it.
        drop(entry);
        let text = format!(
            "job_done {}",
            json!({"job_id": job, "exit_code": exit, "lines": lines, "tail": tail})
        );
        let owner = AgentId::from(owner_s.as_str());
        deliver(&work, &owner, &owner_s, &text, &cid, born, false, true).await;
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
    deliver_answer(kernel, self_id, client_id, &asker, &open.reply_client, &text).await;
}

/// The answer, sent AS THIS AGENT into the asker's conversation through
/// [`Kernel::call`]: an AI asker acks once the row is stored, and nothing waits. A
/// REFUSED answer is not dropped: this agent is told why, as a notice. Python
/// `_deliver_answer`.
async fn deliver_answer(
    kernel: &Arc<Kernel>,
    self_id: &str,
    client_id: &str,
    asker: &str,
    reply_client: &str,
    text: &str,
) {
    let (k, me, cid, who) = (
        Arc::clone(kernel),
        AgentId::from(self_id),
        client_id.to_string(),
        asker.to_string(),
    );
    crate::life::owe(&me, 1);
    let out = kernel
        .call(
            Item::new(
                AgentId::from(asker),
                json!({"type": "send", "text": text, "client_id": reply_client}),
            ),
            move |out| async move {
                answer_refused(&k, &me, &cid, &who, &out).await;
                crate::life::owe(&me, -1);
            },
        )
        .await;
    if out.status != Status::Accepted {
        let me = AgentId::from(self_id);
        answer_refused(kernel, &me, client_id, asker, &out).await;
        crate::life::owe(&me, -1);
    }
}

/// An inquiry answer the asker did not take: this agent is told, as a notice.
async fn answer_refused(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    client_id: &str,
    asker: &str,
    out: &fantastic_kernel::Outcome,
) {
    let err = fantastic_kernel::refusal::error_text(&out.result);
    if err.is_some() || matches!(out.status, Status::Failed | Status::TimedOut) {
        let why = err.unwrap_or_else(|| out.status.as_str().to_string());
        crate::life::notice(
            kernel,
            self_id,
            &format!("[your inquiry answer did not reach {asker}: {why}]"),
            client_id,
        )
        .await;
    }
}

/// The model's `inquiry_answer`: delivered to the asker of the inquiry this pass's
/// messages belong to (`clients`, the first that has one open). `Some` is the error
/// block for the model — nothing to answer, or the placeholder. Python
/// `_answer_inquiry`.
pub(crate) async fn answer_inquiry(
    kernel: &Arc<Kernel>,
    self_id: &str,
    clients: &[String],
    call: &crate::tool_parse::ToolCall,
) -> Option<String> {
    let open = {
        let open = open_inquiries().lock().expect("open inquiries poisoned");
        clients
            .iter()
            .find(|c| open.contains_key(&(self_id.to_string(), (*c).clone())))
            .cloned()
    };
    let Some(client_id) = open else {
        return Some(
            "inquiry_answer: nothing asked you in this run — an answer goes to an inquiry you \
             received (`[inquiry from …]`)"
                .to_string(),
        );
    };
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
    close_inquiry(kernel, self_id, &client_id, body).await;
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

/// What one pass's routing takes: its answer and where it came from.
pub(crate) struct Routed {
    /// The answer's raw text (thinking marked).
    pub(crate) text: String,
    /// The client id of the pass.
    pub(crate) client_id: String,
    /// The client ids of every message the pass read: an inquiry answer goes to the
    /// first that has one open.
    pub(crate) clients: Vec<String>,
    /// The AI layer's per-call deadline — the KERNEL defaults nothing; the model may
    /// override it per call with a `timeout` arg in the envelope.
    pub(crate) send_timeout: f64,
    /// The backend's window ([`crate::agent_loop::BackendConfig::model_window`]): the
    /// inline cap a reply body is measured against.
    pub(crate) model_window: i64,
    /// The answer's row: every reply's `↑` and batch token.
    pub(crate) message_id: Option<i64>,
    /// Every closed call part of the pass, `(stream_id, body)`: a dispatched message
    /// carries the `stream_id` of the part it was streamed as.
    pub(crate) call_streams: Vec<(String, String)>,
    /// The model stopped with a block open: the pass sends nothing, its closed calls
    /// included.
    pub(crate) cut: bool,
}

/// Honour where this pass's answer goes. THE MODEL'S CALLS ARE THE AGENT'S CALLS: this
/// runs in a task the agent's pass spawned (`Kernel::spawn_task`), so every call is the
/// agent's and is gated as the agent. It ends once every call is OUT: each reply comes
/// later, as a message.
pub(crate) async fn route_reply(self_id: AgentId, kernel: Arc<Kernel>, routed: Routed) {
    let Routed {
        text,
        client_id,
        clients,
        send_timeout,
        model_window,
        message_id,
        call_streams,
        cut,
    } = routed;
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
    // Everything owed to a destination, in the order the model wrote it: each reply is
    // its own message, tagged with its place in the batch (`k of M`).
    let mut units: Vec<Unit> = Vec::new();

    // `stop` (#766, 1.8): the model says the work cannot be done, and why. The reason of
    // its first `stop` that has one; the reader refused a `stop` without one (S25).
    let stop_called: Option<String> = calls.iter().filter(|c| c.name == "stop").find_map(|c| {
        c.arguments
            .get("reason")
            .and_then(Value::as_str)
            .filter(|r| !r.trim().is_empty())
            .map(str::to_string)
    });
    // EVERY CALL GOES OUT AT ONCE, each through `Kernel::call` (concurrent,
    // side-effect order across calls undefined), and each reply is ONE message when
    // it comes (#795, Q3): never held until the batch is whole, because that would be
    // a wait. The tag's `k of M` says how much of the batch a message is.
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
        if let Some(err) = answer_inquiry(&kernel, self_id.as_str(), &clients, call).await {
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
        // EVERY dispatched call carries the layer's deadline. The KERNEL defaults
        // nothing, so without it a call to a handler that never returns would never
        // reply — no timeout block, the work simply stops. Set BEFORE the model's own `timeout` arg, which
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
        item = item.with_reply_to(match &reply_to {
            Some(rt) => fantastic_kernel::ReplyTo::Agent(AgentId::from(rt.as_str())),
            None => fantastic_kernel::ReplyTo::Row,
        });
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

    // THE TAIL: an answer reaching here has calls (the attempts re-asked one with none,
    // #795), or it was cut and dispatched nothing. It ends with `done` or `stop`, and
    // the next message wakes the agent.
    let meta = crate::helpers::agent_meta(&self_id, &kernel);
    let batch = Batch {
        self_id: self_id.clone(),
        client_id: client_id.clone(),
        message_id,
        // A FRESH token per pass, shared across destinations; the answer's own id when
        // it has one, so every `↑` and every `batch` names a row the model can see.
        token: message_id.map_or_else(mint_batch_token, |t| t.to_string()),
        cap: crate::context::inline_cap(&meta, model_window),
    };
    let owed: HashMap<String, usize> = units.iter().fold(HashMap::new(), |mut m, u| {
        *m.entry(u.dest.clone()).or_insert(0) += 1;
        m
    });
    // EVERY DESTINATION'S INCARNATION, READ NOW, when the routing decides: a target that
    // reboots meanwhile is still judged against the incarnation this pass addressed.
    let born: HashMap<String, Born> = owed.keys().map(|d| (d.clone(), Born::of(d))).collect();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut said: Vec<(String, Vec<String>)> = Vec::new();
    let mut by_item: HashMap<usize, Owed> = HashMap::new();
    for u in units {
        let k = {
            let n = seen.entry(u.dest.clone()).or_insert(0);
            *n += 1;
            *n
        };
        let m = owed.get(&u.dest).copied().unwrap_or(1);
        match u.idx {
            // THIS LAYER'S OWN TEXT (a refusal before dispatch): one message per
            // destination, now
            None => {
                let block = batch.block("-", &u.target, &u.verb, u.title.as_deref(), k, m, &u.body.unwrap_or_default());
                match said.iter_mut().find(|(d, _)| *d == u.dest) {
                    Some((_, b)) => b.push(block),
                    None => said.push((u.dest, vec![block])),
                }
            }
            Some(i) => {
                by_item.insert(i, Owed { dest: u.dest, target: u.target, verb: u.verb, title: u.title, k, m });
            }
        }
    }
    for (dest, blocks) in said {
        // a message to SELF made only of this layer's own text is a notice
        let notice = dest == self_id.as_str();
        let b = born.get(&dest).copied().unwrap_or_default();
        deliver(&kernel, &self_id, &dest, &blocks.join("\n\n"), &client_id, b, notice, true).await;
    }
    let batch = Arc::new(batch);
    for (i, item) in items.into_iter().enumerate() {
        let owed = by_item.remove(&i);
        let born = owed.as_ref().map_or(Born::default(), |o| born.get(&o.dest).copied().unwrap_or_default());
        let reply = Reply {
            batch: Arc::clone(&batch),
            owed,
            born,
            deadline: deadlines[i].clone(),
            sent: payloads.get(i).cloned().unwrap_or_default(),
        };
        crate::life::owe(&self_id, 1);
        let (k, r) = (Arc::clone(&kernel), reply.clone());
        let out = kernel
            .call(item, move |out| async move { r.take(&k, out).await })
            .await;
        if out.status != Status::Accepted {
            // REFUSED AT LAUNCH (the gate, the target): its reply is the refusal, now
            reply.take(&kernel, out).await;
        }
    }
    if let Some(reason) = stop_called.as_deref() {
        // THE MODEL STOPPED (#766, 1.8): the work cannot be done. Status `failed`,
        // `reason: gave_up`, the model's reason as `error`, the open inquiry closed with
        // it; the calls written beside it were dispatched above, as beside `done`.
        give_up(&kernel, &self_id, &client_id, reason).await;
    }
    // `done`: nothing more from this agent now; the next message wakes it, an answer
    // owed to an inquiry it received included (#795, Q5).
}

/// One pass's routing, as each of its replies needs it.
struct Batch {
    self_id: AgentId,
    client_id: String,
    message_id: Option<i64>,
    token: String,
    cap: i64,
}

impl Batch {
    /// ONE reply block, tagged — the ONE line a reply is known by, forever (in the
    /// store, in `recall` pages, and as the stub a stripped body leaves):
    /// `[reply to <pid> · <target> <verb> · "<title>" · ↑<answer> · k of M in batch <b>]`.
    #[allow(clippy::too_many_arguments)]
    fn block(&self, pid: &str, target: &str, verb: &str, title: Option<&str>, k: usize, m: usize, body: &str) -> String {
        let mut tag = format!("[reply to {pid} \u{b7} {target} {verb}");
        if let Some(t) = title {
            tag.push_str(&format!(" \u{b7} \"{t}\""));
        }
        if let Some(t) = self.message_id {
            tag.push_str(&format!(" \u{b7} \u{2191}{t}"));
        }
        if m > 1 {
            tag.push_str(&format!(" \u{b7} {k} of {m} in batch {}]", self.token));
        } else {
            tag.push(']');
        }
        format!("{tag}\n{body}")
    }
}

/// Where one dispatched call's reply goes, and its place in the batch.
#[derive(Clone)]
struct Owed {
    dest: String,
    target: String,
    verb: String,
    title: Option<String>,
    k: usize,
    m: usize,
}

/// One dispatched call, as its settle needs it.
#[derive(Clone)]
struct Reply {
    batch: Arc<Batch>,
    /// Where the reply goes (`None`: `reply_to: null`, the row only).
    owed: Option<Owed>,
    born: Born,
    /// The deadline the call ran under, as the model is told it.
    deadline: String,
    /// What the call was sent: its target as the model wrote it, and its payload.
    sent: (String, Value),
}

impl Reply {
    /// THE CALL SETTLED: an exec job it started is watched (#306), and the reply is
    /// rendered and delivered to its `reply_to` as one message. An ack of a message to an
    /// AI is stored without a wake (Q2). The call is no longer owed.
    async fn take(&self, kernel: &Arc<Kernel>, out: fantastic_kernel::Outcome) {
        let me = &self.batch.self_id;
        let (exec, sent) = &self.sent;
        let started = sent.get("type").and_then(Value::as_str) == Some("start")
            && out.result.get("status").and_then(Value::as_str) == Some("running");
        if let Some(job) = out.result.get("job_id").filter(|v| !v.is_null()) {
            if started {
                watch_job(kernel, me.as_str(), exec, job, &self.batch.client_id);
            }
        }
        if let Some(o) = &self.owed {
            let body = self.render(kernel, &out, o);
            let pid = out.pid.map_or_else(|| "-".to_string(), |t| t.0.to_string());
            let text = self.batch.block(&pid, &o.target, &o.verb, o.title.as_deref(), o.k, o.m, &body);
            let ack = o.verb == "send" && out.status == Status::Completed && is_ack_text(&text);
            deliver(kernel, me, &o.dest, &text, &self.batch.client_id, self.born, false, !ack).await;
        }
        crate::life::owe(me, -1);
    }

    /// The reply a model reads — INCLUDING an error: a call that failed silently is the
    /// one thing a model can never debug. Over the inline cap, the body is held in RAM
    /// under its pid and replaced by its ORIGIN line.
    fn render(&self, kernel: &Arc<Kernel>, out: &fantastic_kernel::Outcome, o: &Owed) -> String {
        let me = &self.batch.self_id;
        let pid = out.pid.map_or_else(|| "-".to_string(), |t| t.0.to_string());
        let mut shown = reply_as_text(out);
        // ANOTHER AGENT'S THINKING NEVER LANDS HERE: a peer's rows are read without it.
        // This agent's own read of its own rows keeps it.
        if o.target != me.as_str() {
            if let Some(clean) = crate::think::without_thinking_fields(&shown) {
                shown = std::borrow::Cow::Owned(clean);
            }
        }
        if o.verb == "reflect" {
            if let Some(trimmed) = hide_view_body(&shown) {
                shown = std::borrow::Cow::Owned(trimmed);
            }
        }
        // A call that passed its deadline went to the dead letter: the sender is told
        // where, and how to resend or drop it.
        let dead_letter = out.status == Status::TimedOut && out.pid.is_some();
        if dead_letter {
            if let Some(obj) = shown.to_mut().as_object_mut() {
                obj.insert(
                    "error".to_string(),
                    json!(format!(
                        "your message to {} hit its {}s deadline \u{2192} dead letter {pid}; resend or drop",
                        o.target, self.deadline
                    )),
                );
            }
        }
        let pointed = point_to_docs(&o.target, &o.verb, &shown);
        let mut body = crate::tool_parse::render_reply(pointed.as_ref().unwrap_or(&shown));
        // FULL ASYNC. A reply is DATA: over the inline cap it is held in RAM under its
        // pid and replaced by its digest + its ORIGIN line. A body is never persisted.
        if let Some(p) = out.pid {
            let over = crate::context::estimate_one(&json!({"role": "user", "content": body})) > self.batch.cap;
            if over && crate::memory::store_reply(me, kernel, p.0, &body) {
                let size = body.chars().count();
                body = format!(
                    "(settled, {size} bytes — NOT inlined, held in memory only \
                     until this agent restarts. origin: {}. To find a line in it: \
                     send yourself <type>grep</type> <query>substring</query> \
                     <ids><item>{}</item></ids>)",
                    origin_line(&out.result, &o.target, &o.verb, &self.sent.1),
                    p.0
                );
            }
        }
        if o.verb == "send" && out.pid.is_some() && !dead_letter {
            body = format!("(deadline {}s)\n{body}", self.deadline);
        }
        body
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

/// A CHAIN CUT BY A RESTART (#726). Everything is records: at its start the kernel
/// settled the calls the previous process left in flight as dead letters
/// ([`fantastic_kernel::executions::CutCall`]); the ones this agent's own tasks made
/// and the messages sent to it reach it here, at its boot, as ONE notice turn of a
/// new chain. A reply it was waiting for will not come (resend or drop it); a message
/// it held or was answering was cut, and its sender is told by its own dead letter.
/// Never silent. A call made inside a dispatch (a pass's own store or provider call)
/// is the pass's plumbing, not the agent's work: the cut message that ran the pass
/// says it. Nothing cut, nothing told.
pub(crate) async fn resume_cut(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    cut: &[fantastic_kernel::executions::CutCall],
) {
    let me = self_id.as_str();
    let lines: Vec<String> = cut
        .iter()
        .filter_map(|c| {
            // A PASS'S OWN REQUEST is its plumbing, not the agent's work: the pass
            // re-runs over its unread messages (#795)
            if c.caller.as_deref() == Some(me) && c.verb.as_deref() == Some("complete") {
                return None;
            }
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
    crate::life::notice(kernel, self_id, &text, &fantastic_kernel::inquiry::default_client_id()).await;
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

/// The give-up tail: say so in the status and close the inquiry.
pub(crate) async fn give_up(kernel: &Arc<Kernel>, self_id: &AgentId, client_id: &str, gave_up: &str) {
    let st = state_for(self_id);
    let mut detail = Map::new();
    detail.insert("reason".to_string(), json!("gave_up"));
    detail.insert("error".to_string(), json!(gave_up));
    emit_status(kernel, &st, self_id, client_id, "failed", detail).await;
    let mut body = Map::new();
    body.insert("error".to_string(), json!(gave_up));
    close_inquiry(kernel, self_id.as_str(), client_id, body).await;
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
