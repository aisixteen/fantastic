//! Reply routing — where ONE provider pass's output goes. Mirrors python
//! `ai_core/core.py:_route_reply`, which is canonical.
//!
//! After one provider pass, its output goes where it was ADDRESSED. Two paths,
//! and the first is the model's:
//!
//!   the model's own `<tool_call>`s — each names a target, a payload and
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

use crate::tool_parse::{extract_tool_calls, malformed_calls};
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

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
pub const NO_TARGET_ERROR: &str = "no target_id, so nothing was dispatched — a send must name the AGENT it goes to. Shape: {\"name\":\"send\",\"arguments\":{\"target_id\":\"<agent>\",\"payload\":{\"type\":\"<verb>\"}}}. Send `{type:'reflect'}` to the kernel for the list of agent ids.";

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
    format!(
        "send to '{target}' carried no payload, so nothing was dispatched. \
         A payload is an object whose `type` names the verb, e.g. {{\"type\":\"reflect\"}}."
    )
}

/// Hand `text` to `target` as a fresh turn. Exactly python's `_deliver`.
async fn deliver(
    kernel: &Arc<Kernel>,
    target: &str,
    text: &str,
    client_id: &str,
    system_prompt: Option<&str>,
) {
    let mut payload = Map::new();
    payload.insert("type".to_string(), json!("send"));
    payload.insert("text".to_string(), json!(text));
    payload.insert("client_id".to_string(), json!(client_id));
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
    let failed = !out.is_completed() || out.result.get("error").and_then(Value::as_str).is_some();
    if failed {
        let why = out
            .result
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("delivery {}", out.status));
        kernel
            .emit(
                &AgentId::from(target),
                json!({
                    "type": "status",
                    "phase": "done",
                    "reason": "error",
                    "error": why,
                    "client_id": client_id,
                }),
            )
            .await;
    }
}

/// python truthiness, for the `target_id or target` fall-through below. A FALSY
/// `target_id` (absent, `null`, `""`, `0`, `{}`, `[]`) falls through to `target`;
/// a truthy non-string one does NOT, and lands on [`NO_TARGET_ERROR`].
fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// The args echoed back with an error, minus `payload` — python's
/// `{k: v for k, v in args.items() if k != "payload"}`. Shows the model the
/// ROUTING it wrote, which is the part that was wrong, without re-quoting a
/// program body at it.
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
/// THE NO-CALL NUDGE — one runtime reminder when a CHAIN turn dispatched
/// nothing. DONE is EXPLICIT: the `{"name":"done"}` tool call ends a chain
/// silently and is never nudged. Why once: the nudge is itself a chain turn,
/// so a second consecutive no-call turn arrives with the nudge as its incoming
/// text — and is NOT nudged again. Mirrors python `_NO_CALL_NUDGE` (canonical;
/// reinstated 2026-09-01 after repo_map:swift twice stopped mid-task).
const NO_CALL_NUDGE: &str =
    "[no call was dispatched] Your turn emitted no <tool_call> envelope, so \
NOTHING ran — a call decided in thinking does not execute unless the envelope is \
written out. If the task is unfinished, emit that call now. If it is finished, end \
the chain explicitly: <tool_call>{\"name\":\"done\"}</tool_call>";

/// Honour where this pass's output goes — the DETACHED routing task. The
/// model's calls are the agent's calls (gated via `with_sender`); `incoming`
/// is the text this turn answered, which decides whether a silent chain turn
/// earns the one no-call nudge.
pub async fn route_reply(
    self_id: AgentId,
    kernel: Arc<Kernel>,
    text: String,
    client_id: String,
    send_to: Option<String>,
    system_prompt: Option<String>,
    incoming: String,
) {
    // THE MODEL'S CALLS ARE THE AGENT'S CALLS — gated. This task is spawned
    // detached, and a tokio spawn does NOT inherit the task-local sender
    // (python's create_task copies contextvars, which is why python was gated
    // and rust was not — measured 2026-09-01, E53_grant: a verb-scoped wall
    // held on python/swift and was silently bypassed here). Re-enter the
    // sender scope for the WHOLE routing pass: dispatches, the binary
    // carrier, and reply deliveries all attribute to the AI agent.
    let sid = self_id.clone();
    fantastic_kernel::send::with_sender(sid, async move {
        route_reply_gated(
            self_id,
            kernel,
            text,
            client_id,
            send_to,
            system_prompt,
            incoming,
        )
        .await
    })
    .await
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
) {
    let sp = system_prompt.as_deref();
    let calls = extract_tool_calls(&text);

    // Everything owed to ONE destination, in the order the model wrote it.
    // Grouping (not a message per call) is the whole point — see below.
    let mut blocks: Vec<(String, String)> = Vec::new();

    // An envelope that OPENED and did not parse gets an error, exactly like a call
    // missing its `target_id` does. Reported FIRST and reported even when other
    // calls in the same pass parsed fine — a partially-read turn dispatched half
    // the model's intent, and which half was lost is not something it can infer
    // from the replies it got. Goes to SELF: `reply_to` lives inside the body we
    // could not read, and the documented default for an absent `reply_to` is self
    // anyway.
    for bad in malformed_calls(&text) {
        blocks.push((self_id.as_str().to_string(), bad.report().to_string()));
    }

    let done_called = calls.iter().any(|c| c.name == "done");
    if !calls.is_empty() {
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
        // Per dispatched item: where its answer goes, and what it called.
        let mut routes: Vec<(Option<String>, String, String)> = Vec::new();

        for call in calls {
            if call.name == "done" {
                // The EXPLICIT end — not a dispatch, not an error, never nudged.
                continue;
            }
            let args = &call.arguments;
            let chosen: Option<&Value> = match args.get("target_id") {
                Some(v) if !is_falsy(v) => Some(v),
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
            let reply_to: Option<String> = match args.get("reply_to") {
                Some(v) => v
                    .as_str()
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
                    blocks.push((rt, body.to_string()));
                }
                continue;
            };
            let Some(Value::Object(payload)) = args.get("payload") else {
                if let Some(rt) = reply_to {
                    let body = json!({
                        "error": no_payload_error(target),
                        "target": target,
                        "you_sent": you_sent(args),
                    });
                    blocks.push((rt, body.to_string()));
                }
                continue;
            };

            let verb = payload
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();

            // send_binary: ONE named payload field is a file's CONTENTS, and a
            // JSON payload has no bytes type — so a stream sink (`write_stream`)
            // that wants raw bytes is otherwise unreachable. The model wrote the
            // program as an ordinary string in `into` (default `bytes`); lift that
            // field out of the header and dispatch it over the kernel's BINARY
            // carrier (the same `send_with_binary` persistence rides), instead of
            // the plain JSON `Item`. Mirrors python `core.py`'s send_binary branch,
            // routed through rust's separate binary channel. Dispatched inline (the
            // carrier is single-target, no ledger id/timeout); the batch path below
            // handles ordinary sends.
            if call.name == "send_binary" {
                let field = args
                    .get("into")
                    .and_then(Value::as_str)
                    .unwrap_or("bytes")
                    .to_string();
                if let Some(Value::String(prog)) = payload.get(&field) {
                    if prog == "@body" {
                        // A LITERAL "@body" is the fence placeholder with NO
                        // fenced block — valid JSON, so it parses and would be
                        // WRITTEN AS THE FILE (measured 2026-09-01, gym:
                        // `illegal character '@'`). Refuse loud instead.
                        if let Some(rt) = reply_to {
                            let err = serde_json::json!({
                                "error": "send_binary NOT dispatched: `bytes` is the \
                                          literal placeholder \"@body\" but no fenced \
                                          block followed. Re-send the SAME call with \
                                          the program between column-0 marker lines \
                                          inside the same <tool_call>: the JSON line, \
                                          then `*** Begin Bytes`, the program verbatim, \
                                          then `*** End Bytes`.",
                                "target": target,
                            });
                            blocks.push((rt, format!("[reply to - · {target} {verb}]\n{err}")));
                        }
                        continue;
                    }
                    let blob = prog.clone().into_bytes();
                    let mut header = payload.clone();
                    header.remove(&field); // rides as the blob, not the JSON header
                    let (reply, _body) = kernel
                        .send_with_binary(&AgentId::from(target), Value::Object(header), blob)
                        .await;
                    if let Some(rt) = reply_to {
                        blocks.push((rt, format!("[reply to - · {target} {verb}]\n{reply}")));
                    }
                    continue;
                }
                // A non-string `into` field can't binary-carry; fall through and
                // dispatch it as an ordinary JSON send (python leaves it as-is too).
            }

            let mut item = Item::new(AgentId::from(target), Value::Object(payload.clone()));
            // The model may NAME the execution (dedup/join across retries) and may
            // bound its own call. The kernel validates both; a bad value settles
            // that one call as a refusal the model can read.
            if let Some(id) = args.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    item = item.with_id(id);
                }
            }
            if let Some(t) = args.get("timeout").and_then(Value::as_f64) {
                item = item.with_timeout(t);
            }
            if let Some(r) = args.get("retries").and_then(Value::as_u64) {
                item = item.with_retries(r as u32);
            }
            items.push(item);
            routes.push((reply_to, target.to_string(), verb));
        }

        let outcomes = if items.is_empty() {
            Vec::new()
        } else {
            kernel.send(items).await
        };

        for (out, (reply_to, target, verb)) in outcomes.into_iter().zip(routes) {
            // Fire-and-forget: it acted, nobody awaits the answer. Deliberately
            // NOT counted in M — the model is owed a count of what it will
            // actually receive.
            let Some(rt) = reply_to else { continue };
            // The result — INCLUDING an error — goes back. A call that failed
            // silently is the one thing a model can never debug.
            //
            // PID first, then target+verb. Target+verb alone is NOT unique: with
            // two calls in flight to one agent `[reply to kernel_state set]` is
            // unmatchable — measured 2026-08-07 (opus re-issued both deletes,
            // then theorised "duplicate delivery" about its own retries) and
            // again 2026-08-20 (the model: "the reply queue is running ahead of me",
            // then re-polled forever). The PID is the same id `ps`/`cancel` use,
            // so a reply names a row the model can act on.
            let pid = out.pid.as_ref().map(|t| t.0.as_str()).unwrap_or("-");
            blocks.push((
                rt,
                format!("[reply to {pid} · {target} {verb}]\n{}", out.result),
            ));
        }
    }

    if !blocks.is_empty() {
        deliver_grouped(&kernel, blocks, &client_id, sp).await;
        return;
    }
    if done_called {
        return; // explicitly finished — nothing more runs, nothing reminds
    }
    // A CHAIN turn (its incoming text was a delivered reply) that dispatched
    // nothing gets ONE reminder — see `NO_CALL_NUDGE`. A chat turn is not a
    // chain, and a turn already answering the nudge is never nudged again.
    // A question turn is the OTHER legitimate silent end (ask + stop; the
    // human's answer resumes the chain) — nudging it would stomp the wait.
    let asked = text.contains("\"type\"") && text.contains("\"question\"");
    if incoming.starts_with("[reply to ")
        && !asked
        && !incoming.starts_with("[no call was dispatched]")
    {
        deliver(&kernel, self_id.as_str(), NO_CALL_NUDGE, &client_id, sp).await;
        return;
    }

    if let Some(to) = send_to.as_deref() {
        deliver(&kernel, to, &text, &client_id, sp).await;
    }
}

/// ONE delivered message per destination: blocks joined by a blank line, each
/// stamped `N of M in batch <token>` when the destination is owed more than one.
///
/// The stamp is for CORRELATION and COMPLETENESS, not "the rest are still in
/// flight" — the batch arrives whole. A model that made 2 calls and got 1 back
/// could not tell "still running" from "answered the wrong one"; that killed
/// three runs (opus 08-07, the model 08-20, the model 08-24).
async fn deliver_grouped(
    kernel: &Arc<Kernel>,
    blocks: Vec<(String, String)>,
    client_id: &str,
    sp: Option<&str>,
) {
    // Destination order = first appearance, so the model reads them in the order
    // it wrote the calls.
    let mut order: Vec<String> = Vec::new();
    let mut by_dest: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (dest, body) in blocks {
        if !by_dest.contains_key(&dest) {
            order.push(dest.clone());
        }
        by_dest.entry(dest).or_default().push(body);
    }
    // One token per TURN, shared across destinations: it names the batch, and a
    // model reading two destinations' messages can see they came from one turn.
    let token = batch_token(&order, &by_dest);
    for dest in order {
        let parts = by_dest.remove(&dest).unwrap_or_default();
        let m = parts.len();
        let body = if m > 1 {
            parts
                .into_iter()
                .enumerate()
                .map(|(i, b)| format!("{b}\n[{} of {m} in batch {token}]", i + 1))
                .collect::<Vec<_>>()
                .join("\n\n")
        } else {
            parts.join("\n\n")
        };
        deliver(kernel, &dest, &body, client_id, sp).await;
    }
}

/// A short, stable-per-turn batch token. Derived from the batch's own content so
/// it needs no clock and no counter — both of which would differ across runtimes
/// and make a cross-runtime byte-diff fail on noise.
fn batch_token(
    order: &[String],
    by_dest: &std::collections::HashMap<String, Vec<String>>,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for d in order {
        d.hash(&mut h);
        if let Some(parts) = by_dest.get(d) {
            parts.len().hash(&mut h);
            for p in parts {
                p.len().hash(&mut h);
            }
        }
    }
    format!("{:04x}", h.finish() as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model-facing strings are PROMPTS, not diagnostics: a model reads them
    /// and retries from them, and all four runtimes must say the same words. Pinned
    /// as LITERAL TEXT so "tidying" one of them shows up as a failing test rather
    /// than as silent cross-runtime drift.
    #[test]
    fn the_model_facing_error_strings_are_verbatim() {
        assert_eq!(
            NO_TARGET_ERROR,
            "no target_id, so nothing was dispatched — a send must name the AGENT \
             it goes to. Shape: {\"name\":\"send\",\"arguments\":{\"target_id\":\
             \"<agent>\",\"payload\":{\"type\":\"<verb>\"}}}. Send \
             `{type:'reflect'}` to the kernel for the list of agent ids."
        );
        assert_eq!(
            no_payload_error("totals"),
            "send to 'totals' carried no payload, so nothing was dispatched. A \
             payload is an object whose `type` names the verb, e.g. \
             {\"type\":\"reflect\"}."
        );
        // The third one is composed by the parser, which OWNS the wording (see
        // `MalformedCall::error` and the literal pin in `tool_parse::tests_malformed`);
        // what this asserts is that the router delivers THAT, verbatim, with nothing
        // reworded on the way through.
        let bad = crate::tool_parse::malformed_calls(
            "<tool_call>{\"name\":\"send\",\"arguments\":{\"code\":\"x = \\\"unclosed}}</tool_call>",
        );
        assert_eq!(bad.len(), 1, "one opened, unparseable envelope: {bad:?}");
        assert!(
            bad[0]
                .error
                .starts_with("tool_call NOT dispatched: invalid JSON — "),
            "verbatim prefix: {:?}",
            bad[0].error
        );
        assert!(
            bad[0].error.ends_with(&format!(
                ". Nothing ran. {}",
                crate::tool_parse::hints::JSON_HINT
            )),
            "verbatim hint: {:?}",
            bad[0].error
        );
        assert_eq!(
            bad[0].report()["error"],
            json!(bad[0].error),
            "the report the router delivers carries that exact sentence"
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
                Some(v) if !is_falsy(v) => Some(v),
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
}
