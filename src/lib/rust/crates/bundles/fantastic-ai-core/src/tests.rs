//! Shared verb/loop/assembly/state tests, driven through `kernel.send`
//! against a `MockProvider` (no HTTP). A real `fantastic-file` agent
//! handles persistence. Each test uses a unique agent id derived from
//! its tempdir so the process-global state map doesn't race.

use super::*;
use crate::agent_loop::BackendConfig;
use crate::events::CallerRoute;
use crate::helpers::safe_client;
use crate::provider::{Provider, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc::Receiver;

const HANDLER_MODULE: &str = "mock_backend.tools";

#[derive(Clone)]
enum ScriptEvent {
    Token(String),
    /// The provider stream fails mid-pass. `run_generation` propagates it, so the
    /// `send` reply carries an `error` — which is what a broken chain must look
    /// like from the router's side.
    Fail(String),
}

/// One agent's scripted generation: the passes it will serve IN ORDER, plus the
/// log of what it was asked. Mirrors python's `_FakeProvider`
/// (`ollama_backend/tests/test_send_to.py`).
///
/// SHARED between every `send` on that agent — the bundle builds a fresh
/// `MockProvider` per dispatch, so the script box must outlive the dispatch or a
/// chain's second turn replays the FIRST pass. It did: the old mock cloned the
/// script per send AND returned an empty pass once exhausted, so an over-eager
/// chain — the exact failure the routing tests exist to catch — silently PASSED
/// with no visible symptom. The request log is the fix: `passes()` is python's
/// `_FakeProvider.calls`, and an unscripted pass is a recorded request with no
/// tokens, not a no-op.
struct MockScript {
    passes: std::sync::Mutex<std::collections::VecDeque<Vec<ScriptEvent>>>,
    /// Every `chat()` served, in order — the model-view message list it was
    /// handed. Turn 2's entry is how a test proves a reply actually came back.
    requests: std::sync::Mutex<Vec<Vec<Value>>>,
    delay: Duration,
}

impl MockScript {
    /// How many provider passes have been served. python's `.calls`.
    fn passes_served(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// The model-view messages handed to pass `n` (0-based), rendered as one
    /// string — what python asserts against as `str(fp.requests[n])`.
    fn request(&self, n: usize) -> String {
        let reqs = self.requests.lock().unwrap();
        let msgs = reqs.get(n).unwrap_or_else(|| {
            panic!(
                "pass {n} never happened — only {} pass(es) served",
                reqs.len()
            )
        });
        Value::Array(msgs.clone()).to_string()
    }
}

/// A provider that replays its agent's shared [`MockScript`], optionally after a
/// per-call delay (to exercise cancel / status mid-flight).
struct MockProvider {
    script: Arc<MockScript>,
}

#[async_trait]
impl Provider for MockProvider {
    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, String> {
        self.script.requests.lock().unwrap().push(messages.to_vec());
        if !self.script.delay.is_zero() {
            tokio::time::sleep(self.script.delay).await;
        }
        let pass = self
            .script
            .passes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        let evs: Vec<Result<ProviderEvent, String>> = pass
            .into_iter()
            .map(|e| match e {
                ScriptEvent::Token(t) => Ok(ProviderEvent::Token(t)),
                ScriptEvent::Fail(e) => Err(e),
            })
            .collect();
        Ok(Box::pin(futures_util::stream::iter(evs)))
    }
    fn model(&self) -> String {
        "test-model".to_string()
    }
}

/// The test bundle: dispatches every verb through ai-core, building a
/// `MockProvider` over the process-global script keyed by agent id.
struct MockBundle;

type ScriptMap = std::collections::HashMap<String, Arc<MockScript>>;

static SCRIPTS: std::sync::OnceLock<std::sync::Mutex<ScriptMap>> = std::sync::OnceLock::new();

fn scripts() -> std::sync::MutexGuard<'static, ScriptMap> {
    SCRIPTS
        .get_or_init(|| std::sync::Mutex::new(ScriptMap::new()))
        .lock()
        .unwrap()
}

/// Script an agent's provider. Returns the SHARED box, so a test reads the pass
/// count and the request log off the same object the bundle serves from.
fn script(agent: &AgentId, passes: Vec<Vec<ScriptEvent>>, delay: Duration) -> Arc<MockScript> {
    let s = Arc::new(MockScript {
        passes: std::sync::Mutex::new(passes.into_iter().collect()),
        requests: std::sync::Mutex::new(Vec::new()),
        delay,
    });
    scripts().insert(agent.as_str().to_string(), Arc::clone(&s));
    s
}

/// The script the bundle serves for this agent — empty (and still LOGGING) when a
/// test scripted none, so an unexpected pass is counted rather than swallowed.
fn script_for(agent: &AgentId) -> Arc<MockScript> {
    let mut map = scripts();
    Arc::clone(map.entry(agent.as_str().to_string()).or_insert_with(|| {
        Arc::new(MockScript {
            passes: std::sync::Mutex::new(std::collections::VecDeque::new()),
            requests: std::sync::Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        })
    }))
}

/// Wait for `n` provider passes. python's `_until_calls`: the chain is
/// asynchronous, so a test polls; failing FAST is the point, since a stalled
/// chain would otherwise wait out the 180s send ceiling.
async fn until_passes(script: &Arc<MockScript>, n: usize) -> bool {
    for _ in 0..250 {
        if script.passes_served() >= n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// `<tool_call>` exactly as [`crate::assembly::SEND_TO_HOWTO`] teaches it.
/// `reply_to`: `Some(v)` writes the key (`"self"`, another id, `"null"`),
/// `None` OMITS it — the two are NOT the same, and which is which is the
/// behaviour half these tests exist for.
fn call_envelope(target: &str, payload: Value, reply_to: Option<&str>) -> String {
    let mut args = Map::new();
    args.insert("target_id".into(), json!(target));
    args.insert("payload".into(), payload);
    if let Some(rt) = reply_to {
        args.insert(
            "reply_to".into(),
            if rt == "null" { Value::Null } else { json!(rt) },
        );
    }
    format!(
        "<tool_call>{}</tool_call>",
        json!({"name": "send", "arguments": Value::Object(args)})
    )
}

/// One pass that emits one whole text chunk.
fn pass(text: impl Into<String>) -> Vec<ScriptEvent> {
    vec![ScriptEvent::Token(text.into())]
}

const CFG: BackendConfig = BackendConfig {
    route: CallerRoute::CliRoundTrip,
    name: "mock_backend",
};

#[async_trait]
impl Bundle for MockBundle {
    fn name(&self) -> &str {
        "mock_backend"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => {
                json!({
                    "id": agent_id.as_str(),
                    "sentence": "Mock LLM agent.",
                    "model": helpers::meta_string_or(agent_id, kernel, "model", "test-model"),
                    "file_bridge_id": helpers::file_bridge_id(agent_id, kernel),
                    "generating": crate::state::is_generating(agent_id),
                    "verbs": {"send": "send", "history": "history"},
                    "emits": {"token": "t"},
                })
            }
            "boot" => Value::Null,
            "send" => {
                let provider: Arc<dyn Provider> = Arc::new(MockProvider {
                    script: script_for(agent_id),
                });
                verbs::send(provider, agent_id, payload, kernel, CFG).await
            }
            "history" => verbs::history(agent_id, payload, kernel).await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            "context_status" => verbs::context_status(agent_id, kernel).await,
            "status" => verbs::status(agent_id, payload),
            other => json!({"error": format!("mock_backend: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }

    /// The backend has no `interrupt` of its own any more: an in-flight `send`
    /// IS a kernel task, killed by the substrate's `cancel`. This is the hook
    /// the kernel runs first — close the turn out for the caller.
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &fantastic_kernel::TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, task_id.as_str(), kernel, CFG.route).await;
        Ok(())
    }
}

fn id_for(tmp: &TempDir, tag: &str) -> String {
    format!(
        "mb_{}_{}",
        tag,
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

async fn mk_kernel(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, MockBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let backend_id = id_for(tmp, tag);
    let file_id = format!("ff_{}", backend_id);
    // Rooted at the loader's own dir so this leg is ALSO the discovered
    // persistence provider (`find_store` matches on the root path).
    add_file_bridge(
        &kernel,
        &file_id,
        &tmp.path().join(".fantastic"),
        &backend_id,
    )
    .await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    (kernel, backend)
}

/// Create a `file_bridge` leg rooted at `root`, opened, and accepting the agent
/// that persists through it.
///
/// Each AI gets its OWN leg: its chat sidecar lands at the LEG's root
/// (`agents/<self>/chat_<client>.jsonl`), so two AIs sharing one leg would
/// interleave under one `agents/` tree.
async fn add_file_bridge(kernel: &Arc<Kernel>, id: &str, root: &std::path::Path, owner: &str) {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": "file_bridge.tools",
                "id": id,
                "root": root.to_string_lossy(),
                // the fs edge seals by default — open it so history persists through it
                "ingress_rule": "allow_all_ingress",
                // Two-sided deny-all: accept the backend that persists through it.
                "callable_by": { owner: "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
}

/// A SECOND backend in the same kernel — routing needs a peer to hand a reply
/// to — with its own leg (see [`add_file_bridge`]) so the two chat stores stay
/// separate files.
async fn add_peer(kernel: &Arc<Kernel>, tmp: &TempDir, primary: &AgentId) -> AgentId {
    let id = format!("{primary}_peer");
    let file_id = format!("ff_{id}");
    add_file_bridge(
        kernel,
        &file_id,
        &tmp.path().join(".fantastic").join(&id),
        &id,
    )
    .await;
    let peer = add_backend(kernel, &id, &file_id).await;
    // WIRING PAIRS WITH GRANTING, both ends: the primary hands replies to the
    // peer and the peer routes back — with the routing pass now GATED (the
    // model's calls are the agent's calls), both edges need both halves.
    for (holder, other) in [
        (primary.clone(), peer.clone()),
        (peer.clone(), primary.clone()),
    ] {
        let ff = format!("ff_{holder}");
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("core"),
                json!({
                    "type": "update_agent",
                    "id": holder.as_str(),
                    // update REPLACES the named field — the stamped baseline
                    // must ride along or self-delivery loses its own egress.
                    "can_call": {
                        "$self": "*", "$parent": "*", "$children": "*",
                        ff: "*", "cli": "*", other.as_str(): "*",
                    },
                    "callable_by": {
                        "$self": "*", "$parent": "*", "$children": "*",
                        other.as_str(): "*",
                    },
                }),
            )])
            .await
            .remove(0)
            .result;
    }
    peer
}

/// Create one backend agent bound to `file_id`. Separate from [`mk_kernel`]
/// because routing needs TWO in one kernel (a peer to hand a reply to).
///
/// `context_window` is EXPLICIT, and that is not decoration: the default is 4096,
/// the default system block now carries `SEND_TO_HOWTO` as well as
/// `CONTEXT_HOWTO`, and a chain's second turn would die on
/// `context_insufficient` — which looks exactly like a routing bug. python
/// documents the same trap at `test_send_to.py:53-66`.
async fn add_backend(kernel: &Arc<Kernel>, id: &str, file_id: &str) -> AgentId {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": id,
                "file_bridge_id": file_id,
                "model": "test-model",
                "context_window": 32768,
                // Deny-all: the backend streams its turn to `cli` and its
                // api-key/blob-era wiring named the leg — both explicit grants
                // (wiring pairs with granting). The chat SIDECAR needs no
                // grant: its reads/writes are bookkeeping, processed from
                // ABOVE (operator context in `history.rs`).
                "can_call": { file_id: "*", "cli": "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    AgentId::from(id)
}

/// Rebind the backend's inbox (CliRoundTrip emits non-cli events there).
fn rebind_backend_inbox(kernel: &Arc<Kernel>, backend: &AgentId) -> Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(kernel.inbox_bound);
    kernel.inboxes.insert(backend.clone(), tx);
    rx
}

async fn drain(rx: &mut Receiver<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..40 {
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        if out.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("done")
        }) {
            while let Ok(v) = rx.try_recv() {
                out.push(v);
            }
            break;
        }
    }
    out
}

#[tokio::test]
async fn reflect_reports_state_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "refl").await;
    let r = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    for key in [
        "id",
        "sentence",
        "model",
        "file_bridge_id",
        "generating",
        "verbs",
    ] {
        assert!(r.get(key).is_some(), "reflect missing key {key:?}: {r:#?}");
    }
    assert_eq!(r["id"], backend.as_str());
    assert_eq!(r["generating"], false);
}

#[tokio::test]
async fn boot_is_noop() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "boot").await;
    let r = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    assert!(
        r.is_null(),
        "boot should be a noop returning null, got {r:?}"
    );
}

#[tokio::test]
async fn send_streams_tokens_and_emits_done() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tok").await;
    script(
        &backend,
        vec![vec![
            ScriptEvent::Token("Hello".into()),
            ScriptEvent::Token(" ".into()),
            ScriptEvent::Token("world".into()),
        ]],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_tok";
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reply["client_id"], client_id);
    assert_eq!(reply["response"], "Hello world");
    assert_eq!(reply["final"], "Hello world");

    let events = drain(&mut rx).await;
    let token_count = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("token"))
        .count();
    assert!(
        token_count >= 3,
        "expected >=3 token events, got {token_count}: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("status")
                && e.get("phase").and_then(Value::as_str) == Some("done")),
        "expected a done event: {events:#?}"
    );
    let phases: std::collections::HashSet<String> = events
        .iter()
        .filter_map(|e| {
            if e.get("type").and_then(Value::as_str) == Some("status") {
                e.get("phase").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    assert!(phases.contains("thinking"), "missing thinking: {phases:?}");
    assert!(
        phases.contains("streaming"),
        "missing streaming: {phases:?}"
    );
    assert!(phases.contains("done"), "missing done: {phases:?}");
}

/// A reasoning turn: the RAW completion (thinking included) is what the caller
/// gets, what the parser reads and what the ROUTER dispatches; the DURABLE turn is
/// stripped. Re-feeding a reasoning model its own deliberation crowds the task out
/// of the window (gemma4:12b, 2026-08-02) — but cutting it before the parse would
/// lose a tool call the model wrote mid-thought, which is what this dispatches.
///
/// `reply_to:null` on the mid-thought call, so the assertion about the DURABLE turn
/// stays about thinking: a default `reply_to` would (correctly) start a second turn
/// and append its own assistant message.
#[tokio::test]
async fn thinking_is_streamed_and_parsed_raw_but_persisted_stripped() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "think").await;
    // The mid-thought call proves DISPATCH by a real filesystem side effect via
    // the file_bridge. String `write` is gone (stream-only now), so probe with a
    // surviving verb that needs no bytes: `mkdir` creates a dir we can stat.
    let probe = tmp.path().join(".fantastic").join("mid_thought");
    let mid_thought_call = call_envelope(
        &format!("ff_{backend}"),
        json!({"type": "mkdir", "path": "mid_thought"}),
        Some("null"),
    );
    script(
        &backend,
        vec![vec![
            ScriptEvent::Token(crate::think::THINK_OPEN.into()),
            ScriptEvent::Token(format!("let me check: {mid_thought_call}")),
            ScriptEvent::Token(crate::think::THINK_CLOSE.into()),
            ScriptEvent::Token("the answer".into()),
        ]],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_think";
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;

    // The REPLY is raw — markers intact, mid-thought call still parseable.
    let raw = reply["response"].as_str().expect("response text");
    assert!(raw.contains(crate::think::THINK_OPEN), "raw reply: {raw:?}");
    assert_eq!(reply["final"], reply["response"]);
    let calls = crate::tool_parse::extract_tool_calls(raw);
    assert_eq!(
        calls.len(),
        1,
        "parser must see the mid-thought call: {raw}"
    );

    // The token stream is raw too — it concatenates to the reply.
    let events = drain(&mut rx).await;
    let streamed: String = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("token"))
        .filter_map(|e| e.get("text").and_then(Value::as_str))
        .collect();
    assert_eq!(streamed, raw);

    // …and it really RAN. Parsing a call the model buried in its deliberation is
    // only worth anything if the router then dispatches it.
    for _ in 0..250 {
        if probe.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(probe.exists(), "the mid-thought call never dispatched");

    // The DURABLE turn is stripped — no marker, no deliberation, no mid-thought
    // call text; just the answer.
    let h = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "history", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    let messages = h["messages"].as_array().expect("messages array");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant");
    let content = last["content"].as_str().unwrap();
    // The REASONING is gone...
    assert!(
        !content.contains("thinking out loud"),
        "deliberation was persisted: {content:?}"
    );
    assert!(content.contains("the answer"));
    // ...but the INVOCATION stays. It dispatched, so history must show the cause
    // of the reply that is about to arrive — a record that hides it makes the
    // model read an effect it never caused.
    assert!(
        content.contains("mid_thought"),
        "the mid-thought call was erased from history: {content:?}"
    );
}

/// A turn spent ENTIRELY reasoning is not an empty completion: the caller still
/// gets the raw text (so a mid-thought tool call dispatches), while durable
/// history holds nothing but the empty conclusion.
#[tokio::test]
async fn a_thinking_only_turn_is_not_an_empty_completion() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "thonly").await;
    script(
        &backend,
        vec![vec![
            ScriptEvent::Token(crate::think::THINK_OPEN.into()),
            ScriptEvent::Token("all 578 tokens went here".into()),
            ScriptEvent::Token(crate::think::THINK_CLOSE.into()),
        ]],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let reply = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": "c_thonly"}),
        )])
        .await
        .remove(0)
        .result;
    let raw = reply["response"].as_str().expect("response text");
    assert!(
        raw.contains("all 578 tokens went here"),
        "the reasoning must reach the caller verbatim: {raw:?}"
    );
    let h = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "history", "client_id": "c_thonly"}),
        )])
        .await
        .remove(0)
        .result;
    let messages = h["messages"].as_array().expect("messages array");
    assert_eq!(messages.last().unwrap()["content"], "");
}

// ── Context Protocol: budget primitives + strategies (no kernel) ─────

fn turn(role: &str, content: &str) -> Value {
    json!({"role": role, "content": content})
}

fn body(n: usize, size: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            turn(role, &format!("turn{i}-{}", "x".repeat(size)))
        })
        .collect()
}

/// A provider that yields one fixed summary token (for `compact`).
struct SummaryProvider(&'static str);
#[async_trait]
impl Provider for SummaryProvider {
    async fn chat(&self, _m: &[Value]) -> Result<ProviderStream, String> {
        let ev = Ok(ProviderEvent::Token(self.0.to_string()));
        Ok(Box::pin(futures_util::stream::iter(vec![ev])))
    }
    fn model(&self) -> String {
        "sum".into()
    }
}

#[test]
fn context_estimate_and_budget() {
    use crate::context::{budget, estimate_one, estimate_tokens, resolve_context_window};
    assert!(estimate_one(&turn("user", &"x".repeat(400))) >= 100);
    assert!(estimate_tokens(&body(4, 100)) > estimate_tokens(&body(2, 100)));
    let mut m = Map::new();
    m.insert("context_window".into(), json!(8000));
    m.insert("num_ctx".into(), json!(2000));
    assert_eq!(resolve_context_window(&m), 8000);
    let mut m2 = Map::new();
    m2.insert("num_ctx".into(), json!(2000));
    assert_eq!(resolve_context_window(&m2), 2000);
    // The default is 131072, not 4096: at 4096 the budget (4096 − 1024 reserve
    // = 3072) no longer fit the kernel's OWN ~3174-token system block, so every
    // agent created without an explicit window failfasted `too_small` before
    // the model was called.
    assert_eq!(
        resolve_context_window(&Map::new()),
        crate::context::DEFAULT_CONTEXT_WINDOW
    );
    assert_eq!(crate::context::DEFAULT_CONTEXT_WINDOW, 131_072);
    let mut tiny = Map::new();
    tiny.insert("context_window".into(), json!(100));
    tiny.insert("output_reserve".into(), json!(1000));
    assert_eq!(budget(&tiny), 256); // floor
}

#[tokio::test]
async fn compact_returns_projection_with_summary() {
    let b = body(10, 80);
    let provider: Arc<dyn Provider> = Arc::new(SummaryProvider("SUMMARY"));
    let proj = crate::strategies::compact(&b, 4, 1_000_000, &provider).await;
    assert_eq!(proj.summary.as_deref(), Some("SUMMARY"));
    assert!(!proj.omitted_marker);
    // recent verbatim, NO fabricated summary turn in the body — plus the PINNED first
    // turn (the task framing, which the overflow split would otherwise take first).
    let mut want = vec![b[0].clone()];
    want.extend(b[b.len() - 4..].to_vec());
    assert_eq!(proj.body, want);
}

#[test]
fn truncate_returns_projection_no_summary() {
    let b = body(10, 80);
    let proj = crate::strategies::truncate(&b, 1_000_000);
    assert!(proj.omitted_marker && proj.summary.is_none());
    assert_eq!(proj.body[0], b[0]); // first kept
    assert_eq!(proj.body[proj.body.len() - 1], b[b.len() - 1]); // live turn kept
}

#[test]
fn context_notice_shapes() {
    let n = crate::projection::context_notice("compact", Some("MY SUMMARY"), false, 7);
    assert_eq!(n["role"], "user");
    assert!(n.get("tool_calls").is_none());
    let c = n["content"].as_str().unwrap();
    assert!(c.contains("[context-notice]") && c.contains("MY SUMMARY"));
    assert!(c.contains("recall") && c.contains("memory agent"));
    assert!(c.contains("7 earlier turn"));
    let t = crate::projection::context_notice("truncate", None, true, 3);
    let tc = t["content"].as_str().unwrap();
    assert!(tc.contains("omitted in place") && !tc.contains("Summary of the dropped span"));
}

// ── Context Protocol: recall + context_status (kernel-driven) ────────

#[tokio::test]
async fn recall_pages_durable_store_and_context_status_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "recall").await;
    script(&backend, vec![pass("noted")], Duration::ZERO);
    // Plant a distinctive fact in the durable store via a normal send.
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "my project codename is HALCYON", "client_id": "chat"}),
        )])
        .await
        .remove(0)
        .result;
    // recall finds it by substring (case-insensitive), full durable history.
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "recall", "query": "codename", "client_id": "chat"}),
        )])
        .await
        .remove(0)
        .result;
    let msgs = r["messages"].as_array().expect("recall messages");
    assert!(
        msgs.iter().any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("halcyon")),
        "recall did not page the planted fact back: {r:#?}"
    );
    assert_eq!(r["client_id"], "chat");

    // context_status shape — default strategy, projection recorded.
    let st = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_status"}),
        )])
        .await
        .remove(0)
        .result;
    for key in [
        "context_window",
        "output_reserve",
        "budget",
        "strategy",
        "last_projection",
        "last_reaction",
    ] {
        assert!(
            st.get(key).is_some(),
            "context_status missing {key:?}: {st:#?}"
        );
    }
    assert_eq!(st["strategy"], "compact");
    // a normal (under-budget) send does not fire compaction
    assert_eq!(st["last_projection"]["fired"], false);
}

#[tokio::test]
async fn history_persists_and_round_trips() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hist").await;
    script(&backend, vec![pass("ack")], Duration::ZERO);
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_hist";
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    let h = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "history", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    let messages = h["messages"].as_array().expect("messages array");
    assert!(
        messages.len() >= 2,
        "expected >=2 messages, got {messages:#?}"
    );
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "ping");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(last["content"], "ack");

    // WHERE it lives: an append-only JSONL sidecar on the AI's OWN leg —
    // `agents/<self>/chat_<client>.jsonl`, one JSON line per turn. NOT a chat
    // agent (durable_memory is gone; memory is plain files) and NOT a loader
    // blob sidecar (that channel routed through whatever provider the LOADER
    // discovered, and dropped every write when none was wired).
    let sidecar = tmp.path().join(".fantastic").join(format!(
        "agents/{}/chat_{}.jsonl",
        backend,
        safe_client(client_id)
    ));
    assert!(
        sidecar.exists(),
        "chat never persisted through the AI's own leg: {sidecar:?}"
    );
    let raw = std::fs::read_to_string(&sidecar).unwrap();
    let turns: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // ONE LINE PER TURN — line index == store index (recall/reaction paging).
    assert_eq!(turns.len(), 2, "one JSONL line per message: {raw:?}");
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(turns[0]["content"], "ping");
    assert_eq!(turns[1]["role"], "assistant");
    assert_eq!(turns[1]["content"], "ack");

    // No chat AGENT exists any more — the sidecar is a file, not a child.
    let chat_id = format!("{backend}_chat");
    assert!(
        !kernel.agents.contains_key(&AgentId::from(chat_id.as_str())),
        "the `<self>_chat` child must be gone: {chat_id}"
    );
}

/// THE REGRESSION THIS PORT EXISTS FOR: chat persists through the AI's OWN
/// `file_bridge_id`, never through whatever provider the LOADER discovered.
///
/// Wired here so the loader has NO provider at all — the root's dir is
/// `<tmp>/.fantastic` and the only `file_bridge` is rooted at `<tmp>/elsewhere`,
/// which `find_store` (it matches on the root path) cannot match. That is a
/// spawned kernel with no `.fantastic`-rooted store, and it is exactly where
/// the old blob channel failed silently: `persist_blob` had nowhere to land, so
/// every chat write vanished, `history` stayed empty forever, and `send` still
/// reported success — which blinded any harness reading transcripts back out of
/// `history`.
#[tokio::test]
async fn chat_persists_with_no_loader_provider_wired() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, MockBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let backend_id = id_for(&tmp, "noprov");
    let file_id = format!("ff_{backend_id}");
    // NOT the loader's dir — no persistence provider is discoverable.
    let leg_root = tmp.path().join("elsewhere");
    add_file_bridge(&kernel, &file_id, &leg_root, &backend_id).await;
    assert!(
        fantastic_kernel::persistence::find_store(&kernel).is_none(),
        "the loader must have NO provider for this test to mean anything"
    );
    let backend = add_backend(&kernel, &backend_id, &file_id).await;

    script(&backend, vec![pass("ack")], Duration::ZERO);
    let _rx2 = rebind_backend_inbox(&kernel, &backend);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping", "client_id": "cli"}),
        )])
        .await
        .remove(0)
        .result;

    let h = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "history", "client_id": "cli"}),
        )])
        .await
        .remove(0)
        .result;
    let messages = h["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        2,
        "history came back empty with no loader provider: {h:#?}"
    );
    assert_eq!(messages[0]["content"], "ping");
    assert_eq!(messages[1]["content"], "ack");
    // …and it is REALLY on disk, in the leg the AI named — the JSONL sidecar.
    let sidecar = leg_root.join(format!("agents/{backend_id}/chat_cli.jsonl"));
    assert!(
        sidecar.exists(),
        "nothing was written through the AI's own leg: {sidecar:?}"
    );
}

/// The backend's bespoke `interrupt` is GONE — an in-flight `send` is a KERNEL
/// TASK, so it is cancelled the way any dispatch is: find it in `ps`, kill it
/// with `cancel`. The bundle's `on_cancel` hook closes the turn out (a `done`
/// event reaches the caller) and the blocked `send` resolves to
/// `{"cancelled": true}` — the substrate's contract, not this bundle's.
#[tokio::test]
async fn kernel_cancel_kills_an_in_flight_send() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "intr").await;
    script(&backend, vec![pass("slow")], Duration::from_secs(2));
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_intr";
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let c = client_id.to_string();
    let send_join = tokio::spawn(async move {
        k.send(vec![Item::new(
            b.clone(),
            json!({"type": "send", "text": "hi", "client_id": c}),
        )])
        .await
        .remove(0)
        .result
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Discover the generation in the live task table…
    let table = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    let task_id = table["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|t| t["agent"] == json!(backend.as_str()) && t["verb"] == json!("send"))
        .and_then(|t| t["id"].as_str())
        .expect("the in-flight send must be in `ps`")
        .to_string();

    // …and kill it with the universal system verb.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": task_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["cancelled"], json!(true));
    assert_eq!(r["killed"], json!([task_id]));

    let reply = send_join.await.expect("send join");
    assert_eq!(reply, json!({"cancelled": true}));

    // The `on_cancel` hook closed the turn out: the caller still gets its `done`.
    let events = drain(&mut rx).await;
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("status")
                && e.get("phase").and_then(Value::as_str) == Some("done")),
        "expected a done event after cancel: {events:#?}",
    );
    // …and the backend no longer reports a phantom generation.
    let st = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["generating"], json!(false));
}

#[tokio::test]
async fn status_during_in_flight_shows_phase() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "stat").await;
    script(&backend, vec![pass("x")], Duration::from_millis(500));
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_stat";
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let c = client_id.to_string();
    let send_join = tokio::spawn(async move {
        k.send(vec![Item::new(
            b.clone(),
            json!({"type": "send", "text": "hi", "client_id": c}),
        )])
        .await
        .remove(0)
        .result
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let snap = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "status", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(snap["generating"], true);
    let cur = &snap["current"];
    assert!(
        cur.is_object(),
        "current should be set during send: {snap:#?}"
    );
    assert_eq!(cur["is_mine"], true);
    let phase = cur["phase"].as_str().unwrap_or("");
    assert!(
        ["thinking", "streaming", "tool_calling"].contains(&phase),
        "unexpected phase {phase:?}: {snap:#?}",
    );
    let _ = send_join.await.expect("send join");
}

// ── reply routing: where ONE pass's output goes ──────────────────────
//
// Mirrors `bundled_agents/ai/ollama/ollama_backend/tests/test_send_to.py`, which
// is canonical. Every agent here sets `context_window` explicitly (see
// `add_backend`) — the default is smaller than the system block once
// `SEND_TO_HOWTO` is in it, and a second turn dying on `context_insufficient`
// looks exactly like a routing bug.

/// THE point of model-owned routing: a one-pass task costs ONE pass. No
/// `<tool_call>` means the model is finished — nothing schedules another turn.
///
/// The second scripted pass is the trap: it must never be served. Before the
/// shared script box existed the mock replayed pass 1 on every send, so an
/// over-eager chain looked identical to a correct stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_call_means_one_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_stop").await;
    let s = script(
        &backend,
        vec![pass("the answer"), pass("must never run")],
        Duration::ZERO,
    );
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["final"], "the answer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        s.passes_served(),
        1,
        "no call must end the turn, got {} passes",
        s.passes_served()
    );
}

/// The model emits a send; it really runs; its REPLY comes back as the next turn,
/// tagged so the model can tell it from a human's message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_call_is_dispatched_and_the_result_returns() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_disp").await;
    let s = script(
        &backend,
        vec![
            pass(call_envelope("core", json!({"type": "list_agents"}), None)),
            pass(r#"done looking <tool_call>{"name":"done"}</tool_call>"#),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "what is here?"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the call's reply never came back"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        s.passes_served(),
        2,
        "one call = one follow-up turn, then it stopped"
    );
    let turn2 = s.request(1);
    // PID, then target AND verb — a bare `[reply to core]` is unmatchable with two in
    // flight, and target+verb alone is not unique either
    assert!(
        turn2.contains(" · core list_agents]") && turn2.contains("[reply to core:"),
        "untagged reply: {turn2}"
    );
    assert!(
        turn2.contains("agents"),
        "the real list_agents result must be in the turn: {turn2}"
    );
}

/// THE regression. A self-addressed reply must QUEUE behind the current turn, not
/// deadlock on the FIFO guard the turn already holds — `BackendState::lock` is a
/// non-reentrant `tokio::sync::Mutex`, so a re-entrant await would stall until the
/// 180s send ceiling. Wrapped in a 5s timeout: failing FAST is the signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_self_chain_does_not_deadlock() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_chain").await;
    let hop = call_envelope("core", json!({"type": "list_agents"}), Some("self"));
    let s = script(
        &backend,
        vec![
            pass(hop.clone()),
            pass(hop),
            pass(r#"enough <tool_call>{"name":"done"}</tool_call>"#),
        ],
        Duration::ZERO,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        let _ = kernel
            .send(vec![Item::new(
                backend.clone(),
                json!({"type": "send", "text": "go"}),
            )])
            .await
            .remove(0)
            .result;
        assert!(
            until_passes(&s, 3).await,
            "chain stalled at {} passes",
            s.passes_served()
        );
    })
    .await
    .expect("a self-addressed chain must not deadlock");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 3, "expected 3 passes");
}

/// The chain carries NO `_call_stack`, and that is deliberate: the guard's own
/// cycle rule would read a `reply_to:self` hop as a loop and kill the chain on
/// turn 2. python's `_deliver` does not carry it either, so its guard is inert on
/// this path by construction.
///
/// Written so the failure mode is unambiguous: the inbound send arrives WITH a
/// chain (`["upstream"]`), so if routing propagated it the follow-up would carry
/// this agent's own id and turn 2 would return `cycle detected` instead of calling
/// the provider. Two passes prove it did not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_self_chain_carries_no_call_stack() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_stack").await;
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                "core",
                json!({"type": "list_agents"}),
                Some("self"),
            )),
            pass("done"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "_call_stack": ["upstream"]}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the follow-up never ran — a propagated _call_stack would refuse it as a cycle"
    );
}

/// The INBOUND half of the guard, which is real parity: a chain that already
/// contains this agent is refused BEFORE the FIFO lock, so A→B→A cannot deadlock
/// on a guard A itself holds. The provider is never reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inbound_cycle_is_refused_before_the_lock() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cyc").await;
    let s = script(&backend, vec![pass("must never run")], Duration::ZERO);
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({
                "type": "send",
                "text": "go",
                "_call_stack": ["upstream", backend.as_str()],
            }),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        r["error"].as_str().unwrap_or("").contains("cycle detected"),
        "expected a cycle refusal: {r:#?}"
    );
    assert_eq!(
        r["cycle"],
        json!(["upstream", backend.as_str(), backend.as_str()]),
        "the refusal hands back the chain as walked"
    );
    assert_eq!(s.passes_served(), 0, "the model must not be called at all");
}

/// AN OMITTED `reply_to` MEANS SELF — the model gets its own answer back.
///
/// The old default was fire-and-forget, and it produced the one failure a model
/// cannot debug: gemma4:12b left the key out of an otherwise perfect call, the
/// send ran, its result was dropped, and the chain died at pass 1 with no reply
/// and no error. Indistinguishable, from inside the model, from being finished.
/// Stopping is still free — it is emitting NO call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_to_omitted_defaults_to_self() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_dflt").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let fo = script(&peer, vec![pass("peer answered")], Duration::ZERO);
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                peer.as_str(),
                json!({"type": "send", "text": "ping"}),
                None,
            )),
            pass("x"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&fo, 1).await,
        "the send itself must still happen"
    );
    assert!(
        until_passes(&s, 2).await,
        "an omitted reply_to must come back to self"
    );
    assert!(
        s.request(1).contains("[reply to "),
        "and arrive tagged as a reply: {}",
        s.request(1)
    );
}

/// Fire-and-forget still exists — it just has to be ASKED for. `reply_to:null`
/// (or `"none"`) acts and ends the chain; that is the right way round, because
/// throwing an answer away is the surprising choice, not the default one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discarding_a_reply_must_be_said_explicitly() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_null").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let fo = script(&peer, vec![pass("peer answered")], Duration::ZERO);
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                peer.as_str(),
                json!({"type": "send", "text": "ping"}),
                Some("null"),
            )),
            pass("x"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&fo, 1).await,
        "the send itself must still happen"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        s.passes_served(),
        1,
        "an explicit null must mean no follow-up turn"
    );
}

/// A call that fails must be VISIBLE to the model — a silent failure is the one
/// thing it can never debug.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_call_comes_back_to_the_model() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_fail").await;
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                "no_such_agent",
                json!({"type": "reflect"}),
                None,
            )),
            pass("ok"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the failure never reached the model"
    );
    let turn2 = s.request(1).to_lowercase();
    assert!(turn2.contains("no_such_agent"), "{turn2}");
    assert!(
        turn2.contains("error") || turn2.contains("unknown"),
        "not an error: {turn2}"
    );
}

/// A `<tool_call>` with no target tells the model so, instead of vanishing — and
/// it names the SHAPE, because a bare fault taught gemma-4-12B nothing about how
/// to retry (2026-08-06).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_with_no_target_is_reported_not_swallowed() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_notgt").await;
    let bad = r#"<tool_call>{"name":"send","arguments":{"reply_to":"self"}}</tool_call>"#;
    let s = script(&backend, vec![pass(bad), pass("ok")], Duration::ZERO);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the bad call was swallowed");
    let turn2 = s.request(1);
    assert!(turn2.contains("no target_id"), "{turn2}");
    assert!(
        turn2.contains("Send `{type:'reflect'}` to the kernel"),
        "it must state the fix, not just the fault: {turn2}"
    );
}

/// Measured 2026-08-06: a payload-less send reached the handler as nothing and
/// came back `'NoneType' object has no attribute 'get'` — an error about OUR
/// internals, naming neither the omitted verb nor the agent. It must say what was
/// missing instead, and must never dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_with_no_payload_names_what_is_missing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_nopl").await;
    let bad = r#"<tool_call>{"name":"send","arguments":{"target_id":"totals"}}</tool_call>"#;
    let s = script(&backend, vec![pass(bad), pass("ok")], Duration::ZERO);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the payload-less send was swallowed"
    );
    let turn2 = s.request(1);
    assert!(turn2.contains("no payload"), "{turn2}");
    assert!(
        turn2.contains("totals"),
        "it must name the agent it was addressed to: {turn2}"
    );
}

/// END TO END, which the parser-level tests do not cover — and the gap that let
/// this ship broken in python: on 2026-08-06 `malformed_calls` returned the right
/// report for gemma's truncated `create_agent` and the model still received
/// nothing, so the run went on believing its agent existed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unparseable_call_reaches_the_model_as_an_error() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_broken").await;
    let broken = concat!(
        r#"<tool_call>{"name":"send","arguments":{"target_id":"kernel","payload":"#,
        r#"{"type":"create_agent","parent_id":"kernel","code":"x = \"unclosed}}</tool_call>"#
    );
    let s = script(&backend, vec![pass(broken), pass("ok")], Duration::ZERO);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the malformed call produced no next turn"
    );
    assert!(s.request(1).contains("NOT dispatched"), "{}", s.request(1));
}

/// Malformed reports come FIRST and come even when a sibling call parsed fine: a
/// partially-read turn dispatched half the model's intent, and which half was lost
/// is not something it can infer from the replies it got.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_call_beside_a_good_one_is_still_reported() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_both").await;
    let text = format!(
        "{}{}",
        call_envelope("core", json!({"type": "list_agents"}), Some("null")),
        r#"<tool_call>{"name":"send","arguments":{"target_id":"n","code":"oops}}"#
    );
    let s = script(&backend, vec![pass(text), pass("ok")], Duration::ZERO);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the broken sibling was swallowed"
    );
    assert!(s.request(1).contains("NOT dispatched"), "{}", s.request(1));
}

/// The other half of the verb-in-the-tool-name fix:
/// `{"name":"reflect","arguments":{"target_id":…}}` is a real instruction and must
/// DISPATCH, not earn an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_named_tool_reaches_the_agent() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_verb").await;
    let call = r#"<tool_call>{"name":"reflect","arguments":{"target_id":"core"}}</tool_call>"#;
    let s = script(&backend, vec![pass(call), pass("ok")], Duration::ZERO);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the reflect never came back");
    let turn2 = s.request(1);
    assert!(
        turn2.contains(" · core reflect]") && turn2.contains("[reply to core:"),
        "{turn2}"
    );
    assert!(!turn2.contains("no payload"), "{turn2}");
}

/// `send_to` delivers this pass's TEXT to a peer — once. It must NOT propagate: a
/// caller cannot spend the model's turns for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_send_to_hands_off_once() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_hand").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let fa = script(
        &a,
        vec![pass("the answer from a"), pass("a must not run again")],
        Duration::ZERO,
    );
    let fb = script(
        &b,
        vec![pass("b replied"), pass("b must not run again")],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe", "send_to": b.as_str()}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&fb, 1).await,
        "the peer never received the reply"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        fb.request(0).contains("the answer from a"),
        "{}",
        fb.request(0)
    );
    assert_eq!(
        (fa.passes_served(), fb.passes_served()),
        (1, 1),
        "send_to must not propagate"
    );
}

/// A failed pass is TERMINAL — a broken chain must not keep spending turns. The
/// script's envelope on pass 2 would chain forever if an errored pass routed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_reply_does_not_route() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_err").await;
    let s = script(
        &backend,
        vec![
            vec![ScriptEvent::Fail("provider exploded".into())],
            pass(call_envelope("core", json!({"type": "list_agents"}), None)),
        ],
        Duration::ZERO,
    );
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        r["error"].as_str().unwrap_or("").contains("exploded"),
        "{r:#?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "an errored pass must not route");
}

/// A handler that DIES must reach the model as `{"error": …}`.
///
/// Two shapes, one contract. A bundle returning `Err` is already mapped to an
/// error reply by the kernel's dispatch, so it routes back on its own; a PANIC is
/// re-raised into the caller, which is why the router catches it. python's
/// equivalent ran bare until 2026-08-02: a `file_bridge` whose ingress rule could
/// not be resolved raised out of its handler, asyncio logged "Task exception was
/// never retrieved", and the model got no reply, no error and no further turn —
/// the run stopped mid-verification and looked finished. An exception is a result;
/// the model can act on one and cannot act on silence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dying_handler_comes_back_as_an_error_not_a_dead_chain() {
    struct Raiser;
    #[async_trait]
    impl Bundle for Raiser {
        fn name(&self) -> &str {
            "raiser"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            match payload.get("type").and_then(Value::as_str) {
                Some("boot") | Some("shutdown") => Ok(None),
                Some("panic") => panic!("the handler died outright"),
                _ => Err("unknown ingress rule type 'password:whatever'".into()),
            }
        }
    }

    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, MockBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel.bundles.register("raiser.tools", Raiser);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let backend_id = id_for(&tmp, "rt_raise");
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(
        &kernel,
        &file_id,
        &tmp.path().join(".fantastic"),
        &backend_id,
    )
    .await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    let raiser = format!("{backend_id}_raiser");
    // Both halves of the edge: the routing pass is GATED (the model's calls
    // are the agent's calls), so the backend needs egress to the raiser and
    // the raiser must accept the backend.
    let _ = kernel
            .send(vec![Item::new(AgentId::from("core"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "raiser.tools", "id": raiser, "callable_by": {backend_id.as_str(): "*"}}))])
            .await
            .remove(0)
            .result;
    let _ = kernel
            .send(vec![Item::new(AgentId::from("core"), json!({"type": "update_agent", "id": backend_id.as_str(), "can_call": {"$self": "*", "$parent": "*", "$children": "*", file_id.as_str(): "*", "cli": "*", raiser.as_str(): "*"}}))])
            .await
            .remove(0)
            .result;

    // ① a bundle that returns Err.
    let s = script(
        &backend,
        vec![
            pass(call_envelope(&raiser, json!({"type": "reflect"}), None)),
            pass("saw it"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the error killed the chain instead of routing"
    );
    let turn2 = s.request(1);
    assert!(
        turn2.contains("password:whatever"),
        "it must carry what actually went wrong: {turn2}"
    );

    // ② a handler that PANICS. Same contract, and the panic must not take the
    //    route task down with it.
    let s2 = script(
        &backend,
        vec![
            pass(call_envelope(&raiser, json!({"type": "panic"}), None)),
            pass("saw the panic too"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "again"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s2, 2).await,
        "the panic killed the chain instead of routing"
    );
    let panic_turn = s2.request(1);
    assert!(
        panic_turn.contains("the handler died outright") && panic_turn.contains("raised"),
        "a panic is a result too: {panic_turn}"
    );
}

/// The default system block may state FACTS — how to reach an agent, what
/// `reflect` returns, where `reply_to` sends the answer. It must NOT name the
/// STRATEGY.
///
/// The removed strategy prompt did exactly that: "either call again or write your
/// final answer to finish", "ORIENT FIRST", "NEVER claim you don't have access".
/// That is the loop, dictated — and reusing that wording because it is "tuned"
/// would quietly void every emergence result taken afterwards. Same twelve banned
/// words as python's `test_prompt_states_the_mechanism_not_a_strategy`.
#[tokio::test]
async fn the_default_prompt_states_the_mechanism_not_a_strategy() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_prompt").await;
    let state = crate::state::state_for(&backend);
    let messages =
        crate::assembly::assemble_messages(&backend, &state, "hi", &kernel, "probe", None).await;
    let sys_block = messages[0]["content"]
        .as_str()
        .expect("system content")
        .to_lowercase();

    // the mechanism IS stated
    assert!(sys_block.contains("reply_to"), "{sys_block}");
    assert!(sys_block.contains("reflect"), "{sys_block}");
    assert!(sys_block.contains("<tool_call>"), "{sys_block}");

    let banned = [
        "loop",
        "iterate",
        "iteration",
        "step by step",
        "keep going",
        "until done",
        "until you are done",
        "agentic",
        "continue working",
        "call again",
        "final answer",
        "to finish",
        "orient first",
    ];
    let leaked: Vec<&str> = banned
        .iter()
        .copied()
        .filter(|w| sys_block.contains(w))
        .collect();
    assert!(
        leaked.is_empty(),
        "system prompt leaks strategy {leaked:?} — voids the test"
    );
}

/// Routing is its OWN `ps` row, labelled `route_reply`.
///
/// Two reasons it is a separate task and not inline work: the row must not be
/// labelled `send` (an operator reading `ps` would see a generation that already
/// finished), and it must survive the dispatch that spawned it — the follow-up turn
/// happens after `send` has returned to its caller. `Kernel::spawn_task` gives both,
/// plus a `CURRENT_TASK` scope so the turns the chain spawns nest under it.
///
/// The peer's provider stalls for half a second, which is what makes the row
/// observable at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routing_is_its_own_ps_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_ps").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let fo = script(&peer, vec![pass("slow peer")], Duration::from_millis(500));
    script(
        &backend,
        vec![pass(call_envelope(
            peer.as_str(),
            json!({"type": "send", "text": "ping"}),
            Some("null"),
        ))],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&fo, 1).await, "the peer was never reached");

    let table = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    let tasks = table["tasks"].as_array().expect("tasks");
    let route_row = tasks
        .iter()
        .find(|t| t["agent"] == json!(backend.as_str()) && t["verb"] == json!("route_reply"));
    assert!(
        route_row.is_some(),
        "routing must be its own `route_reply` row: {tasks:#?}"
    );
    assert!(
        !tasks
            .iter()
            .any(|t| t["agent"] == json!(backend.as_str()) && t["verb"] == json!("send")),
        "the generation is finished — nothing may still be labelled `send`: {tasks:#?}"
    );
}

// ─── the end-of-chain contract: DONE is explicit; silence is nudged ─────
// Reinstated 2026-09-01 (mirrors python `test_done_and_nudge.py`).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_chain_turn_is_nudged_once_then_ends() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ndg").await;
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                "kernel",
                json!({"type": "reflect", "tree": "none"}),
                None,
            )),
            pass("thinking out loud, no envelope"),
            pass("still just prose"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 3).await, "the nudge never arrived");
    // request(n) is the WHOLE messages json; the nudge must be its final turn.
    assert!(
        s.request(2)
            .contains("[no call was dispatched] Your turn emitted no"),
        "pass 3 must be the nudge"
    );
    // the nudged-silent turn ended the chain — no fourth pass
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_done_call_ends_the_chain_without_a_nudge() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ndg2").await;
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                "kernel",
                json!({"type": "reflect", "tree": "none"}),
                None,
            )),
            pass(r#"finished. <tool_call>{"name":"done"}</tool_call>"#),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 2, "done must end the chain silently");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_turn_is_never_nudged() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ndg3").await;
    let s = script(
        &backend,
        vec![pass("just an answer, no calls")],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi"}),
        )])
        .await
        .remove(0)
        .result;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 1, "prose to a person is the product");
}
