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
use fantastic_kernel::{AgentId, Kernel};
use futures_util::FutureExt;
use serde_json::{json, Map, Value};
use std::panic::AssertUnwindSafe;
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
    let _ = kernel
        .send(&AgentId::from(target), Value::Object(payload))
        .await;
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

/// `kernel.send`, with a handler that DIES turned into a result the model can act
/// on.
///
/// A handler that raises must not take the chain down with it. Python's
/// equivalent ran bare until 2026-08-02, when a `file_bridge` whose ingress rule
/// could not be resolved raised out of its handler: the exception escaped the
/// router, asyncio logged "Task exception was never retrieved", and the model got
/// no reply, no error, and no further turn — the run simply stopped
/// mid-verification and looked finished. An exception is a result too; the model
/// can act on `{"error": …}` and can act on nothing at all.
///
/// In THIS runtime a bundle that returns `Err` already arrives as
/// `{"error": …}` (the kernel's dispatch maps it), so the case left to cover is a
/// PANIC, which [`fantastic_kernel::Kernel::send`] re-raises into its caller.
/// `catch_unwind` here and NOT an inner `tokio::spawn`: a spawn loses the task-
/// locals, so the row would land parentless and escape `cancel`. Note a panic
/// still poisons any kernel mutex the handler held — today's `resume_unwind` has
/// the same consequence, and this does not fix that.
async fn dispatch_guarded(
    kernel: &Arc<Kernel>,
    target: &str,
    payload: Value,
    pid: &mut Option<fantastic_kernel::tasks::TaskId>,
) -> Value {
    let target_id = AgentId::from(target);
    let fut = kernel.send_traced(&target_id, payload, Some(pid));
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(reply) => reply,
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_string());
            json!({"error": format!("panic: {what}"), "raised": true, "target": target})
        }
    }
}

/// Honour where this pass's output goes. Runs DETACHED, after the FIFO guard is
/// dropped — see the module header.
///
/// Takes owned values because it is spawned as its own kernel task and must be
/// `'static`.
pub async fn route_reply(
    self_id: AgentId,
    kernel: Arc<Kernel>,
    text: String,
    client_id: String,
    send_to: Option<String>,
    system_prompt: Option<String>,
) {
    let sp = system_prompt.as_deref();
    let calls = extract_tool_calls(&text);
    // An envelope that OPENED and did not parse gets an error, exactly like a call
    // missing its `target_id` does. Reported FIRST and reported even when other
    // calls in the same pass parsed fine — a partially-read turn dispatched half
    // the model's intent, and which half was lost is not something it can infer
    // from the replies it got. Goes to SELF: `reply_to` lives inside the body we
    // could not read, and the documented default for an absent `reply_to` is self
    // anyway.
    for bad in malformed_calls(&text) {
        deliver(
            &kernel,
            self_id.as_str(),
            &bad.report().to_string(),
            &client_id,
            sp,
        )
        .await;
    }
    if !calls.is_empty() {
        for call in calls {
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
                if let Some(rt) = reply_to.as_deref() {
                    let body = json!({"error": NO_TARGET_ERROR, "you_sent": you_sent(args)});
                    deliver(&kernel, rt, &body.to_string(), &client_id, sp).await;
                }
                continue;
            };
            let Some(Value::Object(payload)) = args.get("payload") else {
                if let Some(rt) = reply_to.as_deref() {
                    let body = json!({
                        "error": no_payload_error(target),
                        "target": target,
                        "you_sent": you_sent(args),
                    });
                    deliver(&kernel, rt, &body.to_string(), &client_id, sp).await;
                }
                continue;
            };
            let mut pid: Option<fantastic_kernel::tasks::TaskId> = None;
            let result =
                dispatch_guarded(&kernel, target, Value::Object(payload.clone()), &mut pid).await;
            // Fire-and-forget: it acted, nobody awaits the answer.
            let Some(rt) = reply_to.as_deref() else {
                continue;
            };
            // The result — INCLUDING an error — goes back. A call that failed
            // silently is the one thing a model can never debug.
            //
            // Target AND verb in the tag (py-canonical shape). With two calls in
            // flight to one agent a bare `[reply to kernel_state]` is unmatchable —
            // measured 2026-08-07 on py: acks lag a turn, the model re-issued both
            // deletes and read its own retries as "duplicate delivery".
            // PID first, then target+verb. Target+verb alone is NOT unique: with two
            // calls in flight to one agent `[reply to kernel_state set]` is unmatchable —
            // measured 2026-08-07 (opus re-issued both deletes, then theorised
            // "duplicate delivery" about its own retries) and again 2026-08-20 (qwen:
            // "the reply queue is running ahead of me", then re-polled forever). The PID
            // is the same id `ps`/`cancel` use, so a reply names a row you can act on.
            let verb = payload.get("type").and_then(Value::as_str).unwrap_or("?");
            let pid = pid.as_ref().map(|t| t.0.as_str()).unwrap_or("-");
            let body = format!("[reply to {pid} · {target} {verb}]\n{result}");
            deliver(&kernel, rt, &body, &client_id, sp).await;
        }
        return;
    }

    if let Some(to) = send_to.as_deref() {
        deliver(&kernel, to, &text, &client_id, sp).await;
    }
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
