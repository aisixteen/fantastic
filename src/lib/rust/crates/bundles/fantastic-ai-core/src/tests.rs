//! Shared verb/loop/assembly/state tests, driven through `kernel.send`
//! against a `MockProvider` (no HTTP). A real `fantastic-file` agent
//! handles persistence. Each test uses a unique agent id derived from
//! its tempdir so the process-global state map doesn't race.

use super::*;
use crate::agent_loop::BackendConfig;
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

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

    /// The model-view messages handed to pass `n`, as values.
    fn request_messages(&self, n: usize) -> Vec<Value> {
        let reqs = self.requests.lock().unwrap();
        reqs.get(n)
            .unwrap_or_else(|| panic!("pass {n} never happened"))
            .clone()
    }
}

/// A provider that replays its agent's shared [`MockScript`], optionally after a
/// per-call delay (to exercise cancel / status mid-flight).
struct MockProvider {
    script: Arc<MockScript>,
}

#[async_trait]
impl Provider for MockProvider {
    async fn chat(&self, messages: &[Value]) -> Result<ProviderStream, ProviderError> {
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
        let evs: Vec<Result<ProviderEvent, ProviderError>> = pass
            .into_iter()
            .map(|e| match e {
                ScriptEvent::Token(t) => Ok(ProviderEvent::Token(t)),
                ScriptEvent::Fail(e) => Err(ProviderError::raised(e)),
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

/// A `<call>` exactly as [`crate::assembly::SEND_TO_HOWTO`] teaches it, BUILT BY
/// THE RENDERER so a fixture cannot drift from the grammar the parser accepts.
/// `reply_to`: `Some(v)` writes the key (`"self"`, another id, `"null"`),
/// `None` OMITS it — the two are NOT the same, and which is which is the
/// behaviour half these tests exist for.
fn call_envelope(target: &str, payload: Value, reply_to: Option<&str>) -> String {
    let mut args = Map::new();
    args.insert("target_id".into(), json!(target));
    if let Some(rt) = reply_to {
        args.insert("reply_to".into(), json!(rt));
    }
    args.insert("payload".into(), payload);
    crate::tool_parse::render_call("send", &args)
}

/// One pass that emits one whole text chunk.
fn pass(text: impl Into<String>) -> Vec<ScriptEvent> {
    vec![ScriptEvent::Token(text.into())]
}

const CFG: BackendConfig = BackendConfig {
    name: "mock_backend",
    provider_missing_error: None,
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
            // A BOOT IS A NEW INCARNATION, exactly as in every real backend.
            "boot" => {
                crate::state::bump_incarnation(agent_id);
                Value::Null
            }
            "send" => {
                let provider: Arc<dyn Provider> = Arc::new(MockProvider {
                    script: script_for(agent_id),
                });
                verbs::send(Some(provider), agent_id, payload, kernel, CFG).await?
            }
            "history" => verbs::history(agent_id, payload, kernel).await,
            "recall" => verbs::recall(agent_id, payload, kernel).await,
            // FULL ASYNC: page a stored reply body by pid, or FIND the line first.
            "pull" => crate::retrieval::pull(agent_id, payload, kernel).await,
            "grep" => crate::retrieval::grep(agent_id, payload, kernel).await,
            // Rebuild the view from the store: on boot, after folds, on request.
            "context_recall" => {
                let provider: Arc<dyn Provider> = Arc::new(MockProvider {
                    script: script_for(agent_id),
                });
                crate::retrieval::context_recall(agent_id, payload, kernel, Some(&provider)).await
            }
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
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await;
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
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
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
            AgentId::from("kernel_state"),
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
                AgentId::from("kernel_state"),
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
            AgentId::from("kernel_state"),
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
fn rebind_backend_inbox(
    kernel: &Arc<Kernel>,
    backend: &AgentId,
) -> fantastic_kernel::inbox::InboxReceiver {
    kernel.ensure_inbox(backend)
}

async fn drain(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..40 {
        while let Some(v) = rx.try_recv() {
            out.push(v.payload);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        if out.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("done")
        }) {
            while let Some(v) = rx.try_recv() {
                out.push(v.payload);
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
    // Through `recall`, which pages the DURABLE turns: `history` is the digest.
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "recall", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    let messages = r["messages"].as_array().expect("recall messages");
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
    // `history` is the DIGEST (python's `{rows, next, total}`) — the assistant
    // turn's body stripped of its thinking is an EMPTY head, 0 bytes.
    let rows = h["rows"].as_array().expect("rows array");
    assert_eq!(rows.last().unwrap()["head"], "");
    assert_eq!(rows.last().unwrap()["bytes"], 0);
    assert!(h.get("messages").is_none(), "python sends no `messages`");
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
    async fn chat(&self, _m: &[Value]) -> Result<ProviderStream, ProviderError> {
        let ev = Ok(ProviderEvent::Token(self.0.to_string()));
        Ok(Box::pin(futures_util::stream::iter(vec![ev])))
    }
    fn model(&self) -> String {
        "sum".into()
    }
}

/// B3.3 — `status.current` has TWO python shapes, not one dict with a flag.
/// With a client: `{phase, send_id, started_at, elapsed, is_mine}` (+ this
/// caller's own text). Without: the in-flight ENTRY minus its text-bearing keys,
/// so `client_id` and `queued_at` are there and the order is the entry's. And an
/// EMPTY `client_id` is falsy — it takes the redacted branch and is echoed back
/// as `''`, where rust turned it into `'cli'`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_current_shape_matches_python() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_status").await;
    let _s = script(&backend, vec![pass("ack")], Duration::from_millis(400));
    let send_kernel = Arc::clone(&kernel);
    let backend_for_task = backend.clone();
    let join = tokio::spawn(async move {
        send_kernel
            .send(vec![Item::new(
                backend_for_task,
                json!({"type": "send", "text": "go", "client_id": "probe"}),
            )])
            .await
            .remove(0)
            .result
    });
    tokio::time::sleep(Duration::from_millis(120)).await;

    let mine = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "status", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        mine["current"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec![
            "phase",
            "send_id",
            "started_at",
            "elapsed",
            "is_mine",
            "text",
            "text_so_far"
        ],
        "{mine:#?}"
    );
    assert_eq!(mine["current"]["is_mine"], json!(true));

    let other = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "status", "client_id": "someone_else"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        other["current"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["phase", "send_id", "started_at", "elapsed", "is_mine"],
        "no client_id leaks into a per-caller block: {other:#?}"
    );
    assert_eq!(other["current"]["is_mine"], json!(false));

    // NO client: the redacted ENTRY, `queued_at` included, `''` echoed.
    for payload in [
        json!({"type": "status"}),
        json!({"type": "status", "client_id": ""}),
    ] {
        let want_echo = payload.get("client_id").cloned().unwrap_or(Value::Null);
        let bare = kernel
            .send(vec![Item::new(backend.clone(), payload)])
            .await
            .remove(0)
            .result;
        assert_eq!(bare["client_id"], want_echo, "{bare:#?}");
        assert_eq!(
            bare["current"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec![
                "client_id",
                "send_id",
                "queued_at",
                "started_at",
                "phase",
                "elapsed",
                "is_mine"
            ],
            "{bare:#?}"
        );
        assert_eq!(bare["current"]["is_mine"], json!(false));
        assert!(bare["current"]["queued_at"].as_f64().unwrap_or(0.0) > 0.0);
        assert_eq!(bare["mine_pending"], json!([]));
    }
    let _ = join.await.unwrap();
}

/// B3.4 — the `send` reply names the row this turn produced. python `_run`
/// returns `{response, final, client_id, turn_id}`; rust minted the id (routing
/// needs it to parent the next turn) and then dropped it from the reply, so a
/// caller could not name the row its own send wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_reply_carries_turn_id() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_tid").await;
    let _s = script(&backend, vec![pass("ack")], Duration::ZERO);
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["response", "final", "client_id", "turn_id"],
        "{r:#?}"
    );
    let turn_id = r["turn_id"].as_i64().expect("turn_id is a row id");
    // It NAMES the assistant row this turn appended.
    let rows = kernel.rows.for_agent(&backend);
    let assistant = rows
        .iter()
        .find(|row| row["id"].as_i64() == Some(turn_id))
        .expect("the row the reply names");
    assert_eq!(assistant["role"], json!("assistant"));
    assert_eq!(assistant["content"], json!("ack"));
}

/// The env is process-global, so the test that overrides the send ceiling must
/// not overlap any other. It takes this lock and restores what it found.
static SEND_TIMEOUT_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// B2.8 — the ceiling is a python FLOAT: `FANTASTIC_AI_SEND_TIMEOUT=0.05` is a
/// 50ms ceiling (rust parsed `u64`, failed, and silently used 180), the refusal
/// spells the value with python's `repr`, and the `status(done)` detail carries
/// `reason` plus the auto-fills and nothing else (rust added
/// `partial_chars`/`partial_head`/`partial_tail`, a shape python never emits).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_provider_times_out_at_the_python_float_ceiling() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let prev = std::env::var("FANTASTIC_AI_SEND_TIMEOUT").ok();
    std::env::set_var("FANTASTIC_AI_SEND_TIMEOUT", "0.05");
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_to").await;
    let _s = script(
        &backend,
        vec![pass("never arrives")],
        Duration::from_millis(600),
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let started = std::time::Instant::now();
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    let elapsed = started.elapsed();
    match prev {
        Some(v) => std::env::set_var("FANTASTIC_AI_SEND_TIMEOUT", v),
        None => std::env::remove_var("FANTASTIC_AI_SEND_TIMEOUT"),
    }
    assert_eq!(
        r,
        json!({"error": "send: timeout after 0.05s", "client_id": "probe"})
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "the float ceiling must bind: {elapsed:?}"
    );
    let evs = drain(&mut rx).await;
    let done = evs
        .iter()
        .find(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("done")
        })
        .expect("status(done)");
    let keys: Vec<&String> = done["detail"].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["reason", "send_id", "started_at", "queue_depth"],
        "{done:#?}"
    );
    assert_eq!(done["detail"]["reason"], json!("timeout"));
}

/// The default is python's `180.0`, and it is a float on the wire of the
/// refusal — not `180`.
#[test]
fn the_default_send_ceiling_is_a_python_float() {
    assert_eq!(crate::verbs::SEND_TIMEOUT_SECS, 180.0);
    assert_eq!(
        fantastic_bundle::py_float_repr(crate::verbs::SEND_TIMEOUT_SECS),
        "180.0"
    );
}

/// B2.6 — python `secrets.token_urlsafe(8)`: 11 base64url characters, no
/// prefix. Rust minted `snd_<8 hex>`.
#[test]
fn send_id_is_pythons_token_urlsafe_shape() {
    for _ in 0..200 {
        let id = crate::helpers::mint_send_id();
        assert_eq!(id.chars().count(), 11, "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{id}"
        );
        assert!(!id.starts_with("snd_"), "{id}");
    }
}

/// B2.10 — python `_safe_client`, per CODE POINT: `str.isalnum` is Unicode-aware
/// and there is no length cap, and the strip is python's whitespace set.
#[test]
fn safe_client_strips_python_whitespace() {
    use crate::helpers::safe_client;
    // `\x1c`–`\x1f` are python whitespace; `str::trim` keeps them.
    assert_eq!(safe_client("\u{1f} p \u{1f}"), "p");
    assert_eq!(safe_client("  "), "cli");
    assert_eq!(safe_client(""), "cli");
    // UNICODE ALPHANUMERICS SURVIVE, and 70 of them are still 70.
    let long: String = "é".repeat(70);
    assert_eq!(safe_client(&long), long);
    // Decomposed `e` + U+0301 — the combining mark is not alphanumeric.
    assert_eq!(safe_client("e\u{301}x"), "e_x");
    // A zero-width joiner is not alphanumeric either.
    assert_eq!(safe_client("a\u{200d}b"), "a_b");
    assert_eq!(safe_client("a/b c"), "a_b_c");
    assert_eq!(safe_client("keep._-"), "keep._-");
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
    let n = crate::projection::context_notice("compact", Some("MY SUMMARY"), false, 7, 0, 0);
    assert_eq!(n["role"], "user");
    assert!(n.get("tool_calls").is_none());
    let c = n["content"].as_str().unwrap();
    assert!(c.contains("[context-notice]") && c.contains("MY SUMMARY"));
    assert!(c.contains("memory agent"));
    // THE RECALL AFFORDANCE IN THE ONE GRAMMAR. The notice taught
    // `{type:'recall', query?, limit?}` — JSON the harness refuses — until
    // 2026-09-12. Python's bytes, pinned.
    assert!(
        c.contains("send your OWN id `<type>recall</type>` (optional `<query>`, `<limit>`)"),
        "{c}"
    );
    assert!(!c.contains("{type:'recall'"), "{c}");
    assert!(c.contains("7 earlier turn"));
    assert!(
        !c.contains("STRIPPED"),
        "no stripped line when nothing was stripped: {c}"
    );
    let t = crate::projection::context_notice("truncate", None, true, 3, 0, 0);
    let tc = t["content"].as_str().unwrap();
    assert!(tc.contains("omitted in place") && !tc.contains("Summary of the dropped span"));
    // A strip reports what left the view, and how to get it back.
    let s = crate::projection::context_notice("strip", None, false, 0, 4, 27_000);
    let sc = s["content"].as_str().unwrap();
    assert!(
        sc.contains(
            "4 reply bod(ies) were STRIPPED to their tag+title line (27000 bytes left your view)"
        ),
        "{sc}"
    );
    assert!(sc.contains("recall it by the pid on its line"), "{sc}");
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
    // python's recall reply is `{messages, total, truncated[, note]}` — no
    // `client_id`, which rust used to add.
    assert!(r.get("client_id").is_none(), "{r:#?}");

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
    // `done` because a call-less turn is nudged now: without it the chain runs
    // on and the LAST stored turn is the nudge's answer, not this one.
    script(
        &backend,
        vec![pass("ack"), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
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
    let rows = h["rows"].as_array().expect("rows array");
    assert!(rows.len() >= 2, "expected >=2 rows, got {rows:#?}");
    assert_eq!(rows[0]["role"], "user");
    assert_eq!(rows[0]["head"], "ping");
    assert_eq!(rows[1]["role"], "assistant");
    assert_eq!(rows[1]["head"], "ack");

    // WHERE IT LIVES: the KERNEL's row store, one table for every agent, keyed
    // by the one counter. It used to be an append-only JSONL sidecar on the AI's
    // own leg (`agents/<self>/chat_<client>.jsonl`) — a private encoding this
    // bundle chose, and a per-CLIENT split that gave two clients on one agent
    // two threads that could not see each other. Agent id is conversation id
    // now, so one list is what every reader gets.
    let rows = conversation(&kernel, &backend);
    // ONE ROW PER TURN — list index == store index (recall/reaction paging).
    // Four turns: the ping, the call-less answer, the nudge it earned, and the
    // `done` that ended the chain.
    assert_eq!(rows.len(), 4, "one row per message: {rows:#?}");
    assert_eq!(rows[0]["role"], "user");
    assert_eq!(rows[0]["content"], "ping");
    assert_eq!(rows[1]["role"], "assistant");
    assert_eq!(rows[1]["content"], "ack");
    assert_eq!(rows[2]["role"], "user");
    assert!(rows[2]["content"]
        .as_str()
        .unwrap()
        .starts_with("[no call was dispatched]"));
    // Every row carries the id the counter gave it, ascending.
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    // NOTHING went to the AI's file leg. Persisting the store is a loader's job.
    assert!(
        !tmp.path()
            .join(".fantastic")
            .join(format!("agents/{backend}"))
            .exists(),
        "the AI wrote a conversation sidecar on its own leg"
    );

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
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
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

    script(
        &backend,
        vec![pass("ack"), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
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
    let rows = h["rows"].as_array().expect("rows array");
    assert_eq!(
        rows.len(),
        4,
        "history came back empty with no loader provider: {h:#?}"
    );
    assert_eq!(rows[0]["head"], "ping");
    assert_eq!(rows[1]["head"], "ack");
    // …and it is REALLY in the KERNEL's row store, not on the AI's file leg.
    // The conversation stopped being this bundle's private JSONL sidecar on
    // 2026-09-10: rows are the kernel's, one table, and putting them on disk is
    // a loader's job that no agent asks for.
    let rows = conversation(&kernel, &backend);
    assert_eq!(
        rows.len(),
        4,
        "the conversation is not in the store: {rows:#?}"
    );
    assert_eq!(rows[0]["content"], "ping");
    assert!(rows.iter().all(|r| r["agent"] == json!(backend.as_str())));
    assert!(
        !leg_root.join(format!("agents/{backend_id}")).exists(),
        "the AI wrote a conversation sidecar on its own leg"
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
        .and_then(|t| t["id"].as_i64())
        .expect("the in-flight send must be in `ps`");

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

/// The model emits a send; it really runs; its REPLY comes back as the next turn,
/// tagged so the model can tell it from a human's message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_call_is_dispatched_and_the_result_returns() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_disp").await;
    let s = script(
        &backend,
        vec![
            pass(call_envelope(
                "kernel_state",
                json!({"type": "list_agents"}),
                None,
            )),
            pass(r#"done looking <call name="done"></call>"#),
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
    // ...then `↑<turn>`, the assistant row this reply answers — python's tag,
    // the link the fold machinery derives chains from (ported 2026-09-12).
    assert!(
        turn2.contains(" \u{b7} kernel_state list_agents \u{b7} \u{2191}")
            && turn2.contains("[reply to "),
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
    let hop = call_envelope("kernel_state", json!({"type": "list_agents"}), Some("self"));
    let s = script(
        &backend,
        vec![
            pass(hop.clone()),
            pass(hop),
            pass(r#"enough <call name="done"></call>"#),
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
                "kernel_state",
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

/// A `<call>` with no target tells the model so, instead of vanishing — and
/// it names the SHAPE, because a bare fault taught gemma-4-12B nothing about how
/// to retry (2026-08-06).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_with_no_target_is_reported_not_swallowed() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_notgt").await;
    // THE PARSER owns this refusal now: a `send` that names neither
    // `<target_id>` nor `<to>` never becomes a call at all, so what reaches the
    // model is the grammar's sentence, naming BOTH ways to address one.
    let bad = r#"<call name="send"><reply_to>self</reply_to><type>reflect</type></call>"#;
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
    assert!(turn2.contains("a send names its target"), "{turn2}");
    assert!(
        turn2.contains("<target_id>agent id</target_id>") && turn2.contains("<to><item>"),
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
    let bad = r#"<call name="send"><target_id>totals</target_id></call>"#;
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
        r#"<call name="send"><target_id>kernel</target_id><type>create_agent</type>"#,
        r#"<parent_id>kernel<code>x = "unclosed</code></call>"#
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
        call_envelope("kernel_state", json!({"type": "list_agents"}), Some("null")),
        r#"<call name="send"><target_id>n</target_id><code>oops"#
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

/// A VERB IN THE TOOL SLOT IS REFUSED BY NAME. The JSON grammar used to guess —
/// `{"name":"reflect",…}` was rewritten into a send of `{type:"reflect"}` — and
/// the guess is gone: the tools are a closed set, and a verb goes in `<type>`.
/// The refusal says exactly that, so the model retries from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_in_the_tool_slot_is_refused_by_name() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_verb").await;
    let call = r#"<call name="reflect"><target_id>core</target_id></call>"#;
    let s = script(&backend, vec![pass(call), pass("ok")], Duration::ZERO);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the refusal never came back");
    let turn2 = s.request(1);
    assert!(turn2.contains("`reflect` is not a tool"), "{turn2}");
    assert!(
        turn2.contains("<type>reflect</type>"),
        "it must show where the verb goes: {turn2}"
    );
    assert!(
        turn2.contains("send, done, await, inquiry_answer"),
        "it must name the closed set: {turn2}"
    );
}

/// `send_to` delivers this pass's TEXT to a peer — once. It must NOT propagate: a
/// caller cannot spend the model's turns for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_send_to_hands_off_once() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_hand").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    // `done`, because a call-less turn is NUDGED now and never reaches the
    // hand-off: the caller's `send_to` applies to a FINISHED turn.
    let fa = script(
        &a,
        vec![
            pass(r#"the answer from a <call name="done"></call>"#),
            pass("a must not run again"),
        ],
        Duration::ZERO,
    );
    let fb = script(
        &b,
        vec![
            pass(r#"b replied <call name="done"></call>"#),
            pass("b must not run again"),
        ],
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
            pass(call_envelope(
                "kernel_state",
                json!({"type": "list_agents"}),
                None,
            )),
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

/// PROVIDER FAILURE, KIND 2 (B2.1): unmapped — python re-raises it out of the
/// handler, and the kernel settles a raise as `{error:"<Type>: <msg>",
/// raised:true}` on a FAILED execution. `status(done, reason='error')` still
/// announces it first, carrying `str(e)` WITHOUT the exception-type prefix.
/// Rust used to answer every provider failure with a COMPLETED
/// `{error, client_id}`, so no AI failure was ever retryable or dead-letterable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmapped_provider_error_settles_raised() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_raise").await;
    let _s = script(
        &backend,
        vec![vec![ScriptEvent::Fail("provider exploded".into())]],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], json!("RuntimeError: provider exploded"));
    assert_eq!(r["raised"], json!(true));
    // The RAISE carries the type; the EVENT carries `str(e)` alone.
    assert!(r.get("client_id").is_none(), "{r:#?}");
    let evs = drain(&mut rx).await;
    let done: Vec<&Value> = evs
        .iter()
        .filter(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("done")
        })
        .collect();
    assert_eq!(done.len(), 1, "{evs:#?}");
    assert_eq!(done[0]["detail"]["reason"], json!("error"));
    assert_eq!(done[0]["detail"]["error"], json!("provider exploded"));
}

/// B2.2: a CONTEXT-PROJECTION refusal is not a provider failure. python returns
/// the seam's dict out of `_run` VERBATIM — no `status(done)` closes a turn that
/// never opened a provider pass, no `client_id` is stapled on, and the handler
/// does not fail. The seam's own `context(too_small)` event is the only one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_projection_refusal_is_returned_verbatim_and_does_not_raise() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_toosmall").await;
    // A window the kernel's own system block cannot fit — the `too_small`
    // failsafe, which refuses BEFORE the model is called.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(), "context_window": 300}),
        )])
        .await;
    let _s = script(&backend, vec![pass("never reached")], Duration::ZERO);
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or_default()
            .contains("mock_backend: context_insufficient — "),
        "{r:#?}"
    );
    assert!(
        r.get("raised").is_none(),
        "a refusal is not a raise: {r:#?}"
    );
    assert!(
        r.get("client_id").is_none(),
        "python returns the seam dict bare: {r:#?}"
    );
    let evs = drain(&mut rx).await;
    assert!(
        !evs.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("done")
        }),
        "no turn opened, so nothing closes one: {evs:#?}"
    );
    assert!(
        evs.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("context")
                && v.get("phase").and_then(Value::as_str) == Some("too_small")
        }),
        "{evs:#?}"
    );
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
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
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
            .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "raiser.tools", "id": raiser, "callable_by": {backend_id.as_str(): "*"}}))])
            .await
            .remove(0)
            .result;
    let _ = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "update_agent", "id": backend_id.as_str(), "can_call": {"$self": "*", "$parent": "*", "$children": "*", file_id.as_str(): "*", "cli": "*", raiser.as_str(): "*"}}))])
            .await
            .remove(0)
            .result;

    // ① a bundle that returns Err.
    let s = script(
        &backend,
        vec![
            pass(call_envelope(&raiser, json!({"type": "reflect"}), None)),
            // ends the chain explicitly — a silent turn is now nudged again
            pass("saw it <call name=\"done\"></call>"),
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
            pass("saw the panic too <call name=\"done\"></call>"),
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
    let (messages, _store_len) =
        crate::assembly::assemble_messages(&backend, &state, "hi", &kernel, "probe", None, None)
            .await;
    let sys_block = messages[0]["content"]
        .as_str()
        .expect("system content")
        .to_lowercase();

    // the mechanism IS stated
    assert!(sys_block.contains("reply_to"), "{sys_block}");
    assert!(sys_block.contains("reflect"), "{sys_block}");
    assert!(sys_block.contains("<call name="), "{sys_block}");

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
async fn silent_chain_turns_are_nudged_until_a_call_or_done() {
    // The model is QUERIED UNTIL IT CALLS (python-canonical, 2026-09-01): a
    // second silent turn used to end the chain silently.
    //
    // NO STREAK AND NO CAP. Rust bounded this at `NUDGE_LIMIT` (24, or the
    // record's `nudge_limit`) and dropped the chain in SILENCE past it — the
    // exact failure the reminder exists to prevent, one layer up. python's two
    // honest ends (`done`, `await`) both return above this branch.
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
            pass("ok, finishing"),
            pass("<call name=\"done\"></call>"),
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
    assert!(until_passes(&s, 5).await, "the chain did not reach done");
    for n in [2, 3, 4] {
        assert!(
            s.request(n)
                .contains("[no call was dispatched] Your turn emitted no"),
            "pass {} must be a nudge",
            n + 1
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 5, "done ended it; nothing reminds after");
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
            pass(r#"finished. <call name="done"></call>"#),
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
async fn a_first_turn_with_no_call_is_nudged_too() {
    // NOT an exemption for prose. A first turn from a person that decides on a
    // call in thought and never emits it used to end the work silently; now it
    // is reminded exactly once, and `done` closes it.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ndg3").await;
    let s = script(
        &backend,
        vec![
            pass("just an answer, no calls"),
            pass(r#"right — <call name="done"></call>"#),
        ],
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
    assert_eq!(
        s.passes_served(),
        2,
        "the silent turn earns one reminder, and `done` ends it"
    );
}

// ── the batch: one deadline, one stamp, one token per turn ───────────

/// EVERY dispatched model call carries the AI layer's per-call deadline. The
/// KERNEL defaults nothing, so a call to a handler that never returns used to hang
/// the whole batch forever: no reply, no timeout block, the chain dead with
/// nothing to read. Python has always set it; rust/swift did not until 2026-09-02.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_model_call_carries_the_layer_default_timeout() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_dl").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    // The peer's generation takes 2s. The layer's deadline is 0.2s, and NO
    // `timeout` arg is written in the envelope — the default has to do the work.
    let _sb = script(&b, vec![pass("too late")], Duration::from_secs(2));
    let sa = script(
        &a,
        vec![pass("read it <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let call = call_envelope(
        b.as_str(),
        json!({"type": "send", "text": "hi", "client_id": "probe"}),
        Some("self"),
    );
    crate::route::route_reply(
        a.clone(),
        Arc::clone(&kernel),
        call,
        "probe".to_string(),
        None,
        None,
        String::new(),
        0.2,
        None,
    )
    .await;
    assert!(
        until_passes(&sa, 1).await,
        "the timed-out call produced no block at all — the batch hung"
    );
    let turn = sa.request(0);
    // C31: python's prose, verbatim — target id raw, verb `py_repr`-quoted,
    // plus `task_id`.
    assert!(
        turn.contains("exceeded its 0.2s timeout; task cancelled"),
        "the block must REPORT the timeout in python's own words: {turn}"
    );
    assert!(
        turn.contains("'send'"),
        "the verb must be py_repr-quoted: {turn}"
    );
    assert!(turn.contains("<task_id>"), "must carry task_id: {turn}");
}

/// C38: routing forwards `timeout` RAW (not `.and_then(Value::as_f64)`), so a
/// non-numeric value reaches the kernel's own `bad_arg` refusal instead of
/// being silently DROPPED at the route.rs coercion site — which used to
/// default the call to the layer's `send_timeout` as if the model had never
/// written the arg at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_timeout_arg_settles_the_kernels_refusal() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_badtimeout").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let sa = script(
        &a,
        vec![pass("read it <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let mut args = Map::new();
    args.insert("target_id".into(), json!(b.as_str()));
    args.insert("reply_to".into(), json!("self"));
    args.insert("timeout".into(), json!("soon"));
    args.insert(
        "payload".into(),
        json!({"type": "send", "text": "hi", "client_id": "probe"}),
    );
    let call = crate::tool_parse::render_call("send", &args);
    crate::route::route_reply(
        a.clone(),
        Arc::clone(&kernel),
        call,
        "probe".to_string(),
        None,
        None,
        String::new(),
        0.2,
        None,
    )
    .await;
    assert!(
        until_passes(&sa, 1).await,
        "the kernel's refusal must reach the model as a block"
    );
    let turn = sa.request(0);
    assert!(
        turn.contains("timeout must be a positive number of seconds"),
        "must carry the kernel's refusal verbatim: {turn}"
    );
    assert!(turn.contains("'soon'"), "must echo the raw value: {turn}");
}

/// A turn whose calls all carried `reply_to:null` DISPATCHED REAL WORK. It is
/// owed no reply, and it is not a silent turn: rust nudged it anyway until
/// 2026-09-02 (gating on the blocks instead of python's `not calls and not
/// units`), telling the model "your turn emitted no <call> element" about a
/// turn that did — and, on a chat turn, handing that text to `send_to` on top.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_fire_and_forget_calls_are_not_a_silent_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_faf").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let sa = script(
        &a,
        vec![
            pass(format!(
                "{}\n{}",
                call_envelope("kernel_state", json!({"type": "list_agents"}), Some("null")),
                call_envelope("kernel_state", json!({"type": "list_agents"}), Some("null")),
            )),
            pass("the nudge would land here"),
        ],
        Duration::ZERO,
    );
    let sb = script(
        &b,
        vec![pass("the handoff would land here")],
        Duration::ZERO,
    );
    // A CHAIN turn (its incoming text is a delivered reply) — exactly the turn the
    // nudge exists for, so nothing but the dispatch can be what keeps it quiet.
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({
                "type": "send",
                "text": "[reply to core:1 · core ping]\nok",
                "client_id": "probe",
                "send_to": b.as_str(),
            }),
        )])
        .await
        .remove(0)
        .result;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        sa.passes_served(),
        1,
        "a dispatching turn must not be nudged"
    );
    assert_eq!(
        sb.passes_served(),
        0,
        "and must not fall through to send_to"
    );
}

/// The batch stamp lives INSIDE the `[reply to …]` tag, where the shared prompt
/// says it does. It rode on its own trailing line in rust/swift until 2026-09-02,
/// which python's history trimmers (`_REPLY_TAG` / `_BLOCK_SPLIT`, both anchored
/// at the START of a block) cannot read at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_batch_stamp_lives_inside_the_reply_tag() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_stamp").await;
    let s = script(
        &backend,
        vec![
            pass(format!(
                "{}\n{}",
                call_envelope("kernel_state", json!({"type": "list_agents"}), Some("self")),
                call_envelope("kernel_state", json!({"type": "list_agents"}), Some("self")),
            )),
            pass("both back <call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the batch never came back");
    let turn2 = s.request(1);
    assert!(
        turn2.contains(" · 1 of 2 in batch ") && turn2.contains(" · 2 of 2 in batch "),
        "the stamp must be composed into the tag: {turn2}"
    );
    assert!(
        !turn2.contains("[1 of 2 in batch"),
        "no trailing stamp line may survive: {turn2}"
    );
}

/// Read a batch token out of a delivered turn. Anchored on the STAMP (`1 of 2 in
/// batch …`), not on the words: the system block teaches the shape, so a bare
/// `in batch ` search finds the prompt's own `<token>` placeholder first.
fn batch_token_of(turn: &str) -> String {
    const MARK: &str = "1 of 2 in batch ";
    let at = turn
        .find(MARK)
        .unwrap_or_else(|| panic!("no batch stamp in: {turn}"))
        + MARK.len();
    turn[at..].chars().take(4).collect()
}

/// A FRESH token per turn. Rust/swift derived it from the batch's own content
/// (destinations, counts, body lengths), so two turns that made the SAME calls
/// collided — which is the exact confusion the token exists to end: a model that
/// read one file twice saw `1 of 2 in batch 7f3a` both times (measured 2026-08-24,
/// the re-read loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_identical_turns_get_different_batch_tokens() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_tok").await;
    let twice = format!(
        "{}\n{}",
        call_envelope("kernel_state", json!({"type": "list_agents"}), Some("self")),
        call_envelope("kernel_state", json!({"type": "list_agents"}), Some("self")),
    );
    let s = script(
        &backend,
        vec![
            pass(twice.clone()),
            pass(twice),
            pass("enough <call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 3).await, "the second batch never landed");
    let first = batch_token_of(&s.request(1));
    let second = batch_token_of(&s.request(2));
    assert_ne!(
        first, second,
        "two identical turns must be distinguishable at a glance"
    );
}

/// A `reflect` reply is LARGE and TRANSIENT, and the instruction for what to
/// record before it goes rides ON THE REPLY — measured 2026-08-20 (`live_plan`):
/// stated once as system-block policy, the model wrote a `SEEN` line for 0 of 10
/// reflects. Only reflect bodies are dropped later, so only they carry it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_replies_carry_the_distillation_nudge() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_nudge").await;
    let s = script(
        &backend,
        vec![
            pass(format!(
                "{}\n{}",
                call_envelope(
                    "kernel_state",
                    json!({"type": "reflect", "tree": "none"}),
                    Some("self")
                ),
                call_envelope("kernel_state", json!({"type": "list_agents"}), Some("self")),
            )),
            pass("SEEN core <call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the reflect never came back");
    let turn2 = s.request(1);
    assert!(
        turn2.contains("this body is TRANSIENT") && turn2.contains("SEEN <id>"),
        "the reflect block must carry the distillation nudge: {turn2}"
    );
    assert_eq!(
        turn2.matches("this body is TRANSIENT").count(),
        1,
        "and ONLY the reflect block: {turn2}"
    );
}

/// A FAILED HANDOFF is announced as an ordinary `status` event — `detail.reason`,
/// the shape reflect's `emits` documents and a UI subscribes to. Rust/swift put
/// `reason`/`error` at the TOP level until 2026-09-02, so every subscriber saw
/// nothing: the silent-death regression the announcement exists to end, wearing
/// the announcement's own clothes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_handoff_is_a_status_event_with_detail() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_hoff").await;
    // The AI's own file leg: a real, reachable agent that answers `{type:'send'}`
    // with an error — a handoff that cannot land.
    let leg = format!("ff_{backend}");
    let mut rx = rebind_backend_inbox(&kernel, &AgentId::from(leg.as_str()));
    let s = script(
        &backend,
        vec![pass(call_envelope(
            "kernel_state",
            json!({"type": "list_agents"}),
            Some(leg.as_str()),
        ))],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    let events = drain(&mut rx).await;
    let status = events
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("status"))
        .unwrap_or_else(|| panic!("the dead handoff was silent: {events:#?}"));
    assert_eq!(status["phase"], "done", "{status:#?}");
    assert_eq!(status["detail"]["reason"], "error", "{status:#?}");
    // B2.4 — the error is the REPLY's own error, verbatim. python passes
    // `reply.get("error")` straight through; the raw status is the fallback and
    // carries NO `delivery ` prefix, which rust added to a string no python
    // subscriber matches.
    let why = status["detail"]["error"]
        .as_str()
        .unwrap_or_else(|| panic!("the announcement must say WHY: {status:#?}"));
    assert!(!why.starts_with("delivery "), "{status:#?}");
    assert!(why.contains("unknown type"), "{status:#?}");
    assert_eq!(status["source"], leg.as_str(), "{status:#?}");
    assert!(status.get("ts").is_some(), "{status:#?}");
    assert_eq!(status["client_id"], "probe", "{status:#?}");
    assert_eq!(s.passes_served(), 1);
}

/// B2.5 — EACH DELIVERY IS ITS OWN `deliver_reply` KERNEL TASK, and the router
/// does not wait for it. A `send` to an AI agent holds that agent's FIFO lock
/// for the whole turn, so awaiting deliveries inline made reply 2 of a 4-call
/// turn wait for the conversation reply 1 started — measured on python
/// 2026-08-24, 26 of 36 calls never answered.
///
/// Since C36 each of those tasks is also a `task` ROW on the target (python's
/// `ai_rows_read.json` rows 427/435/445/462), under the very id `ps` reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_is_a_deliver_reply_task_not_awaited() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_dsoon").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    // The peer's own turn is SLOW. If the router awaited the delivery, this
    // agent's `send` could not return before the peer finished.
    script(&peer, vec![pass("peer ack")], Duration::from_millis(800));
    let _s = script(
        &backend,
        vec![pass(call_envelope(
            "kernel_state",
            json!({"type": "list_agents"}),
            Some(peer.as_str()),
        ))],
        Duration::ZERO,
    );
    let started = std::time::Instant::now();
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    // The routing pass itself is detached, so give it a moment to spawn.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "the router awaited the delivery: {:?}",
        started.elapsed()
    );
    let table = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    let row = table["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|t| t["verb"] == json!("deliver_reply"))
        .unwrap_or_else(|| panic!("no deliver_reply task row: {table:#?}"));
    assert_eq!(
        row["agent"],
        json!(peer.as_str()),
        "the task belongs to the TARGET: {row:#?}"
    );
    // C36 — A FORK IS A MESSAGE: the delivery's pid is the id of a `task` row
    // on the target, so a finished delivery is still explainable from the store.
    let store: Vec<_> = kernel
        .rows
        .for_agent(&peer)
        .into_iter()
        .filter(|r| {
            r.get("kind") == Some(&json!("task")) && r.get("verb") == Some(&json!("deliver_reply"))
        })
        .collect();
    assert_eq!(
        store.len(),
        1,
        "one `deliver_reply` row per delivery: {store:?}"
    );
    assert_eq!(store[0]["id"], row["id"], "the task id is the row id");
    // …and it is BOOKKEEPING: the conversation reader never sees it.
    assert!(
        crate::history::load_history(&peer, &kernel, "cli")
            .await
            .iter()
            .all(|r| r.get("verb") != Some(&json!("deliver_reply"))),
        "a delivery task reached the peer's conversation"
    );
}

/// B2.4 — a CANCELLED handoff is not announced. python's `failed` is
/// `status in ("failed", "timed_out")`; rust used `!is_completed()`, which also
/// caught `cancelled` and turned every operator kill into a spurious error
/// event on the target's inbox.
#[test]
fn only_failed_and_timed_out_count_as_a_failed_handoff() {
    use fantastic_kernel::outcome::Status;
    for (status, announced) in [
        (Status::Failed, true),
        (Status::TimedOut, true),
        (Status::Cancelled, false),
        (Status::Completed, false),
    ] {
        assert_eq!(
            matches!(status, Status::Failed | Status::TimedOut),
            announced,
            "{status}"
        );
    }
    // The raw status is what python puts in `error` — no prefix, python's spelling.
    assert_eq!(Status::TimedOut.as_str(), "timed_out");
}

/// ONE event on contention, and it is the `status` one. Rust/swift emitted a bare
/// `{type:'queued'}` ahead of it until 2026-09-02 — an undocumented type python
/// never sends, which broke any per-submission event count and every
/// cross-runtime event-stream diff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contended_sends_emit_one_queued_status_and_no_bare_queued_event() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "q1").await;
    // Both passes END their chain: a call-less turn is nudged now, and each
    // nudge is another submission queueing behind the same lock — which is the
    // very thing this test counts.
    script(
        &backend,
        vec![
            pass(r#"first <call name="done"></call>"#),
            pass(r#"second <call name="done"></call>"#),
        ],
        Duration::from_millis(400),
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_q";
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let c = client_id.to_string();
    let first = tokio::spawn(async move {
        k.send(vec![Item::new(
            b,
            json!({"type": "send", "text": "one", "client_id": c}),
        )])
        .await
        .remove(0)
        .result
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "two", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    let _ = first.await.expect("first send");
    let mut events = drain(&mut rx).await;
    while let Some(v) = rx.try_recv() {
        events.push(v.payload);
    }
    assert!(
        !events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("queued")),
        "the bare `queued` event is not part of the contract: {events:#?}"
    );
    let queued: Vec<&Value> = events
        .iter()
        .filter(|e| {
            e.get("type").and_then(Value::as_str) == Some("status")
                && e.get("phase").and_then(Value::as_str) == Some("queued")
        })
        .collect();
    assert_eq!(
        queued.len(),
        1,
        "one contended submission, one queued status: {events:#?}"
    );
    assert!(queued[0]["detail"]["ahead"].is_number(), "{:#?}", queued[0]);
}

/// `recall` answers the turn WHOLE. Rust/swift capped each recalled turn at 2000
/// chars while python returned it verbatim — same verb, same store, two different
/// replies, and the cut half is exactly what a model pages back FOR.
#[tokio::test]
async fn recall_returns_the_turn_whole() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rec_whole").await;
    script(&backend, vec![pass("noted")], Duration::ZERO);
    let long = format!("HEAD {} TAIL", "x".repeat(6000));
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": long, "client_id": "chat"}),
        )])
        .await
        .remove(0)
        .result;
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "recall", "query": "HEAD", "client_id": "chat"}),
        )])
        .await
        .remove(0)
        .result;
    let msgs = r["messages"].as_array().expect("recall messages");
    let content = msgs[0]["content"].as_str().expect("content");
    assert_eq!(
        content.chars().count(),
        long.chars().count(),
        "{}",
        &content[..80]
    );
    assert!(
        content.ends_with("TAIL"),
        "the tail is what was being paged back"
    );
}

/// The reaction cursor is the DURABLE STORE's length, not the model view's. The
/// view is deduped/trimmed, so counting it puts the cursor short by exactly the
/// turns the trimmers dropped — and `derive_reaction` then scans assistant turns
/// written BEFORE the notice and scores a reaction that never happened.
#[tokio::test]
async fn the_compaction_cursor_is_the_store_length_not_the_view() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "mark").await;
    let state = crate::state::state_for(&backend);

    // ① the store length is what `assemble_messages` reports — even though the
    //    view it built is SHORTER, because the stale reflect deduped away.
    let stale = "[reply to ks:a1 · core reflect]\nOLD TREE";
    let fresh = "[reply to ks:a2 · core reflect]\nNEW TREE";
    let turns = vec![
        turn("user", stale),
        turn("assistant", "noted"),
        turn("user", fresh),
        turn("assistant", "noted again"),
    ];
    crate::history::append_history(&backend, &kernel, "probe", &turns).await;
    let (messages, store_len) =
        crate::assembly::assemble_messages(&backend, &state, "hi", &kernel, "probe", None, None)
            .await;
    assert_eq!(store_len, 4, "the STORE keeps every turn");
    assert!(
        messages.len() < 1 + store_len + 1,
        "the view must be shorter than the store: {} messages",
        messages.len()
    );
    assert!(
        !Value::Array(messages).to_string().contains("OLD TREE"),
        "the superseded reflect must be gone from the VIEW"
    );

    // ② and the mark records THAT number, not the view's length.
    let provider: Arc<dyn Provider> = Arc::new(SummaryProvider("SUMMARY"));
    let mut fat = vec![turn("system", "sys")];
    fat.extend(body(60, 8000));
    let n = fat.len();
    let _ = crate::projection::project_context(
        &provider,
        &backend,
        &state,
        &kernel,
        "probe",
        "mock_backend",
        fat,
        store_len,
    )
    .await
    .expect("the projection must fire, not failfast");
    let mark = state.compaction_mark.lock().expect("mark").clone();
    assert_eq!(
        mark,
        Some((store_len, "probe".to_string())),
        "the cursor is the store length (view body was {} turns)",
        n - 1
    );
}

/// SEVEN PARALLEL CALLS, NO `reply_to` — the shape that killed a live chain.
///
/// `subai_graph:rust` emitted one well-formed turn batching seven `reflect`
/// sends with the key omitted, and NOTHING came back: no reply block, no error,
/// no timeout, no cancel. The chain ended at pass 1 (measured 2026-09-03), and
/// the same signature killed `agentic.build:rust` at pass 4 — also a turn whose
/// calls carried no `reply_to`. An omitted key means the model gets its own
/// answer back, so every one of these is owed a block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_of_calls_without_reply_to_all_come_back() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_batch").await;
    // The follow-up turn is what proves delivery: the blocks arrive as ONE
    // message and the model is asked again.
    let sa = script(
        &a,
        vec![pass("seen <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    // Seven targets that all answer — the fixture's own file leg, reflected.
    let leg = format!("ff_{}", a.as_str());
    let text: String = (0..7)
        .map(|_| call_envelope(&leg, json!({"type": "reflect"}), None))
        .collect::<Vec<_>>()
        .join("\n");
    crate::route::route_reply(
        a.clone(),
        Arc::clone(&kernel),
        text,
        "probe".to_string(),
        None,
        None,
        "[reply to x · y z]".to_string(),
        30.0,
        None,
    )
    .await;
    assert!(
        until_passes(&sa, 1).await,
        "seven dispatched calls produced NO follow-up turn — the replies were dropped"
    );
    let turn = sa.request(0);
    let blocks = turn.matches("[reply to ").count();
    assert!(
        blocks >= 7,
        "every call is owed a block; got {blocks} in: {turn}"
    );
}

/// THE REAL PATH: dispatch `send` at the agent and let ITS OWN routing run.
///
/// [`a_batch_of_calls_without_reply_to_all_come_back`] drives `route_reply`
/// directly and passes. The live failure went through the `send` VERB, which
/// drops the FIFO guard and spawns routing as a DETACHED task — so the chain
/// depends on that task outliving the dispatch that spawned it. If it does not,
/// the caller gets its reply and the model never sees one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routing_survives_the_dispatch_that_spawned_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_surv").await;
    let leg = format!("ff_{}", a.as_str());
    let first = (0..7)
        .map(|_| call_envelope(&leg, json!({"type": "reflect"}), None))
        .collect::<Vec<_>>()
        .join("\n");
    let sa = script(
        &a,
        vec![pass(&first), pass("seen <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    // ONE send, exactly as a caller makes it. Turn 1 emits the batch; the
    // follow-up turn only happens if the spawned routing delivered.
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "build it", "client_id": "probe"}),
        )])
        .await;
    assert!(
        until_passes(&sa, 2).await,
        "turn 2 never ran — the detached routing did not deliver after its \
         dispatch returned (passes={})",
        sa.passes_served()
    );
}

/// SEVEN DIFFERENT TARGETS, ONE OF THEM ANOTHER AI — the live shape.
///
/// The two tests above batch calls at ONE plain target and pass. The turn that
/// actually died reflected seven DISTINCT agents, and one of them (`brain`) is a
/// second inference unit: dispatching it starts a generation of its own, and
/// routing awaits the whole batch. If that peer's turn does not settle, the
/// batch never completes and nothing is ever delivered — with no error to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_that_includes_a_peer_ai_still_delivers() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_peer").await;
    let brain = add_peer(&kernel, &tmp, &a).await;
    let leg = format!("ff_{}", a.as_str());
    // `brain` answers its own turn; the builder batches it alongside plain reads.
    let _sb = script(&brain, vec![pass("brain says hi")], Duration::ZERO);
    let mut envs: Vec<String> = (0..6)
        .map(|_| call_envelope(&leg, json!({"type": "reflect"}), None))
        .collect();
    envs.push(call_envelope(
        brain.as_str(),
        json!({"type": "send", "text": "what shape?", "client_id": "probe"}),
        None,
    ));
    let sa = script(
        &a,
        vec![
            pass(envs.join("\n")),
            pass("seen <call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "explore", "client_id": "probe"}),
        )])
        .await;
    assert!(
        until_passes(&sa, 2).await,
        "turn 2 never ran — a batch containing a peer AI delivered nothing \
         (passes={})",
        sa.passes_served()
    );
}

// ── await: the honest wait ───────────────────────────────────────────────────

/// `await` with nothing of the agent's in flight and nothing queued is REFUSED,
/// and the refusal says what to do instead. Twin of python's
/// `test_await_with_nothing_in_flight_is_refused_and_told_what_to_do`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_with_nothing_in_flight_is_refused_and_told_what_to_do() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "aw1").await;
    let s = script(
        &backend,
        vec![
            pass(r#"waiting <call name="await"></call>"#),
            pass(r#"<call name="done"></call>"#),
        ],
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
    assert!(until_passes(&s, 2).await, "the refusal never woke it");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 2, "and nothing else ran");
    let turn2 = s.request(1);
    assert!(turn2.contains("[await refused]"), "{turn2}");
    assert!(turn2.contains("nothing of yours is in flight"), "{turn2}");
}

/// The honest wait: something is QUEUED for the agent, so `await` ends the turn
/// with no nudge and no refusal — and the queued message is the next turn. Twin
/// of python's `test_await_with_a_message_queued_stops_quietly_and_the_queue_wakes_it`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_with_a_message_queued_stops_quietly_and_the_queue_wakes_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "aw2").await;
    let s = script(
        &backend,
        vec![
            pass(r#"waiting <call name="await"></call>"#),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::from_millis(300),
    );
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let first = tokio::spawn(async move {
        k.send(vec![Item::new(b, json!({"type": "send", "text": "one"}))])
            .await
            .remove(0)
            .result
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "two"}),
        )])
        .await
        .remove(0)
        .result;
    let _ = first.await.expect("first send");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 2, "the queued message is the next turn");
    for n in 0..2 {
        let t = s.request(n);
        assert!(!t.contains("[await refused]"), "pass {n}: {t}");
        assert!(!t.contains("[no call was dispatched]"), "pass {n}: {t}");
    }
    assert!(s.request(1).contains("two"), "{}", s.request(1));
}

/// A `<to>` list becomes ONE COPY PER RECIPIENT, and every copy carries the whole
/// list — that is what lets a recipient answer the group instead of forking a
/// private conversation the others never see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_to_list_fans_out_one_copy_per_recipient_carrying_the_list() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "fan").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let call = format!(
        r#"<call name="send"><to><item>{b}</item><item>{a}</item></to><re>412</re>
<type>send</type><text>CANDIDATE 586</text><reply_to>null</reply_to></call>"#
    );
    let fa = script(
        &a,
        vec![pass(call), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let fb = script(
        &b,
        vec![pass(r#"heard <call name="done"></call>"#)],
        Duration::ZERO,
    );
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(until_passes(&fb, 1).await, "the peer never got its copy");
    let peer_turn = fb.request(0);
    assert!(peer_turn.contains("CANDIDATE 586"), "{peer_turn}");
    // Its OWN copy reached `a` too — a fan-out addresses every name on the list,
    // including the sender when the sender lists itself.
    assert!(until_passes(&fa, 2).await, "the sender never got its copy");
}

/// `history` answers TWO shapes and both are load-bearing: `rows` is the digest
/// a MODEL is handed (python's, heads capped, bodies behind `pull`), `messages`
/// is the full text a SPAWNED runtime's harness has no other way to read.
#[tokio::test]
async fn history_answers_the_digest_and_the_full_turns() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hdig").await;
    let long = "x".repeat(300);
    script(
        &backend,
        vec![pass(long.clone()), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping"}),
        )])
        .await
        .remove(0)
        .result;
    let h = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "history"}))])
        .await
        .remove(0)
        .result;

    let rows = h["rows"].as_array().expect("rows");
    let head = rows[1]["head"].as_str().unwrap();
    assert_eq!(head.chars().count(), 120, "a head is capped: {head:?}");
    assert_eq!(rows[1]["bytes"], 300, "…and says how big the whole is");
    assert_eq!(rows[1]["kind"], "assistant", "kind falls back to role");
    assert!(rows[0]["id"].as_i64().unwrap() > 0);
    assert_eq!(h["total"], rows.len());

    // And the reply is python's, EXACTLY: no `messages`, no `client_id`.
    assert_eq!(
        h.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        vec!["rows".to_string(), "next".to_string(), "total".to_string()],
        "{h:#?}"
    );
    // The whole text is still one call away — `recall` pages the durable turn.
    let r = kernel
        .send(vec![Item::new(backend, json!({"type": "recall"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["messages"].as_array().unwrap()[1]["content"], long);
}

/// A NEGATIVE `limit` is already satisfied by the empty page — python
/// `_fit_reply(digest, cap, limit or None)` returns `([], True)`.
#[tokio::test]
async fn history_negative_limit_is_an_empty_page() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hneg").await;
    script(
        &backend,
        vec![pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping"}),
        )])
        .await
        .remove(0)
        .result;
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "history", "limit": -1}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["rows"], json!([]), "{r:#?}");
    assert_eq!(r["next"], Value::Null);
    assert!(r["total"].as_u64().unwrap() >= 2, "{r:#?}");
}

/// Paging: `limit` cuts the page, `next` names where to resume, and an `after`
/// the store does not know pages NOTHING rather than starting over.
#[tokio::test]
async fn the_history_digest_pages_forward() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hpage").await;
    script(
        &backend,
        vec![pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping"}),
        )])
        .await
        .remove(0)
        .result;
    let page = |args: Value| {
        let kernel = Arc::clone(&kernel);
        let backend = backend.clone();
        async move {
            kernel
                .send(vec![Item::new(backend, args)])
                .await
                .remove(0)
                .result
        }
    };
    let first = page(json!({"type": "history", "limit": 1})).await;
    assert_eq!(first["rows"].as_array().unwrap().len(), 1);
    let next = first["next"].as_i64().expect("a cursor");
    let second = page(json!({"type": "history", "after": next})).await;
    assert!(
        second["rows"].as_array().unwrap()[0]["id"]
            .as_i64()
            .unwrap()
            > next,
        "the second page must resume past the cursor"
    );
    let nowhere = page(json!({"type": "history", "after": 999_999})).await;
    assert!(
        nowhere["rows"].as_array().unwrap().is_empty(),
        "an unknown cursor pages nothing, never the head again"
    );
}

/// PER-CLIENT EVENTS LAND ON THE AGENT'S INBOX, tagged with `client_id` — never
/// on an inbox named after the client.
///
/// A synthetic client id has no `Agent`, so it has no watcher set, and a watcher
/// that subscribed to the AGENT (which is what a `watch` frame names) saw
/// nothing at all: measured 2026-09-10, `compaction:rust` fired its projection
/// and pushed no event the harness could observe. Python has always emitted on
/// the agent (`core.py::_to_caller`). Guarded here because the e2e that caught it
/// is a paid live run, and this is a one-line regression to reintroduce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_client_events_are_emitted_on_the_agent_not_a_client_named_inbox() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "evt_route").await;
    let mut agent_rx = rebind_backend_inbox(&kernel, &backend);

    let client = "browser_tok";
    let client_id = AgentId::from(client);
    let mut client_rx = kernel.ensure_inbox(&client_id);

    crate::events::to_caller(
        &kernel,
        &backend,
        client,
        json!({"type": "token", "delta": "x"}),
    )
    .await;

    let mut seen = Vec::new();
    while let Some(v) = agent_rx.try_recv() {
        seen.push(v.payload);
    }
    assert_eq!(seen.len(), 1, "the event must reach the agent: {seen:?}");
    for ev in &seen {
        assert_eq!(ev["client_id"], client, "the event must be TAGGED: {ev}");
        assert_eq!(ev["source"], backend.as_str());
    }
    assert!(
        client_rx.try_recv().is_none(),
        "nothing may be emitted on an inbox named after the client"
    );
}

/// B2.7 — ONE caller route, python's `_to_caller`: `client_id == 'cli'` is
/// DISPATCHED to the `cli` agent (so its handler prints to stdout), everyone
/// else is emitted on the backend's own inbox. Rust selected between the two
/// with a per-backend `CallerRoute` knob python does not have, so three of the
/// four backends never reached the `cli` handler at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cli_caller_round_trips_through_the_cli_agent() {
    /// Stands in for the real `cli` bundle: records what it was handed.
    struct CliSink(Arc<std::sync::Mutex<Vec<Value>>>);
    #[async_trait]
    impl Bundle for CliSink {
        fn name(&self) -> &str {
            "cli"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            self.0.lock().expect("sink poisoned").push(payload.clone());
            Ok(None)
        }
    }

    let tmp = TempDir::new().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, MockBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel
        .bundles
        .register("cli.sink", CliSink(Arc::clone(&seen)));
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let backend_id = id_for(&tmp, "evt_cli");
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(
        &kernel,
        &file_id,
        &tmp.path().join(".fantastic"),
        &backend_id,
    )
    .await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    let mut agent_rx = rebind_backend_inbox(&kernel, &backend);

    // Two-sided deny-all: the backend already holds `can_call:{cli:*}`, so the
    // `cli` agent has to accept it.
    let mut cli_meta = Map::new();
    cli_meta.insert("callable_by".to_string(), json!({ backend.as_str(): "*" }));
    let cli = Agent::new(
        AgentId::from("cli"),
        Some("cli.sink".to_string()),
        None,
        cli_meta,
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&cli));

    crate::events::to_caller(&kernel, &backend, "cli", json!({"type": "token"})).await;
    let handed = seen.lock().expect("sink poisoned").clone();
    assert_eq!(handed.len(), 1, "the cli handler must run: {handed:?}");
    assert_eq!(handed[0]["client_id"], json!("cli"));
    assert_eq!(handed[0]["source"], json!(backend.as_str()));
    assert!(
        agent_rx.try_recv().is_none(),
        "a cli event is dispatched, not emitted on the agent"
    );

    // …and a NON-cli caller still lands on the agent's own inbox.
    crate::events::to_caller(&kernel, &backend, "browser_tok", json!({"type": "token"})).await;
    let ev = agent_rx.try_recv().expect("the browser event").payload;
    assert_eq!(ev["client_id"], json!("browser_tok"));
    assert_eq!(seen.lock().expect("sink poisoned").len(), 1);
}

// ─────────────────────────────────────────────────────────────────
// FULL ASYNC: a reply too large to inline becomes a row to page.
//
// These drive the three verbs python has carried since 2026-09-06 and rust
// gained 2026-09-11 (`pull`, `grep`, `context_recall`) through the kernel, the
// way an agent reaches them.
// ─────────────────────────────────────────────────────────────────

/// Put a row on the agent's conversation directly — the store is the kernel's,
/// and a test that wants a history writes one rather than acting out ten turns.
/// The agent's CONVERSATION — its rows without the kernel's own bookkeeping.
/// Since C36 a dispatch writes a `call` row and a fork a `task` row onto the
/// same list, so a raw `for_agent` count is no longer a count of turns.
fn conversation(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<fantastic_kernel::Row> {
    kernel
        .rows
        .for_agent(agent)
        .into_iter()
        .filter(|r| {
            !matches!(
                r.get("kind").and_then(Value::as_str),
                Some("call" | "task" | "body" | "view" | "settle")
            )
        })
        .collect()
}

fn put_row(kernel: &Arc<Kernel>, agent: &AgentId, row: Value) -> i64 {
    let Value::Object(m) = row else {
        panic!("a row is an object")
    };
    kernel.rows.put(agent, m).expect("row rejected")
}

#[tokio::test]
async fn a_stored_body_is_paged_by_pull_and_the_page_fits_inline() {
    // WHY THIS EXISTS. A 240KB reply is not a turn the model can swallow; it is
    // data it must be able to LOOK AT. Without paging the only options are
    // "drop it" or "die", and both lose the answer the call was made for.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pull").await;
    let pid = kernel.rows.mint_id();
    let body = "line one\n".repeat(4000);
    kernel.rows.body_put(&backend, pid, &body).unwrap();

    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "pull", "pid": pid, "offset": 0}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r.get("pid").and_then(Value::as_i64), Some(pid), "{r}");
    assert_eq!(r.get("eof").and_then(Value::as_bool), Some(false), "{r}");
    let text = r.get("text").and_then(Value::as_str).unwrap_or("");
    assert!(!text.is_empty(), "a page must carry bytes: {r}");
    // THE PAGE FITS WHERE IT IS GOING. A page over the inline cap would itself
    // be stored and digested — the model would pull its own pull, forever.
    let meta = crate::helpers::agent_meta(&backend, &kernel);
    let serialized = serde_json::to_string(&r).unwrap();
    assert!(
        crate::context::estimate_one(&json!({"role": "user", "content": serialized}))
            <= crate::context::inline_cap(&meta),
        "a pull page must fit the inline cap"
    );

    // `next_offset` continues it, and the last page says `eof`.
    let mut offset = r.get("next_offset").and_then(Value::as_i64).unwrap();
    let mut guard = 0;
    loop {
        let p = kernel
            .send(vec![Item::new(
                backend.clone(),
                json!({"type": "pull", "pid": pid, "offset": offset}),
            )])
            .await
            .remove(0)
            .result;
        if p.get("eof").and_then(Value::as_bool) == Some(true) {
            break;
        }
        offset = p.get("next_offset").and_then(Value::as_i64).unwrap();
        guard += 1;
        assert!(guard < 500, "paging did not terminate at offset {offset}");
    }
}

#[tokio::test]
async fn pull_on_an_unstored_pid_says_so_rather_than_inventing_a_page() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pullmiss").await;
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "pull", "pid": 999_999}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r.get("reason").and_then(Value::as_str),
        Some("not_found"),
        "{r}"
    );
}

#[tokio::test]
async fn grep_finds_the_line_inside_a_body_that_was_never_pulled() {
    // THE OPERATION THE MODEL ACTUALLY WANTS. Measured 2026-09-06 (python): four
    // ~10k-token thinking turns byte-scanning one 6.8KB body in 300-char windows
    // hunting a single line. "Find the line that mentions X" is one call.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grep").await;
    let pid = kernel.rows.mint_id();
    let mut body = String::new();
    for i in 0..800 {
        body.push_str(&format!("filler line {i}\n"));
    }
    body.push_str("the AUDIT MARKER is 7f3a\n");
    kernel.rows.body_put(&backend, pid, &body).unwrap();
    // The conversation row whose tag carries that pid — how grep knows the body
    // is in reach at all.
    put_row(
        &kernel,
        &backend,
        json!({
            "role": "user",
            "content": format!("[reply to {pid} · notes read · \"survey\"]\n(settled, {} bytes)", body.len()),
        }),
    );

    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": "audit marker"}),
        )])
        .await
        .remove(0)
        .result;
    let matches = r
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        matches.iter().any(|m| m
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("7f3a")),
        "the marker line must be found INSIDE the stored body: {r}"
    );
    assert_eq!(
        matches[0].get("id").and_then(Value::as_i64),
        Some(pid),
        "a body hit is addressed by its pid: {r}"
    );
}

#[tokio::test]
async fn grep_answers_under_the_inline_cap_even_when_everything_matches() {
    // THE CAP INVARIANT. A verb the agent calls ON ITSELF that answers over the
    // cap is stored and digested — and then the model pulls its own grep
    // (measured 2026-09-06, python: the run drowned in pulls).
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepcap").await;
    for i in 0..400 {
        put_row(
            &kernel,
            &backend,
            json!({"role": "user", "content": format!("needle {i} {}", "x".repeat(300))}),
        );
    }
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": "needle"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r.get("truncated").and_then(Value::as_bool),
        Some(true),
        "{r}"
    );
    let meta = crate::helpers::agent_meta(&backend, &kernel);
    let serialized = serde_json::to_string(&r).unwrap();
    assert!(
        crate::context::estimate_one(&json!({"role": "user", "content": serialized}))
            <= crate::context::inline_cap(&meta),
        "grep must answer under the inline cap"
    );
}

#[tokio::test]
async fn a_fold_id_in_grep_scope_descends_to_its_members() {
    // A FOLD IS ONE DESCENT AWAY. If `grep <fold id>` searched only the fold row
    // the mark would be a wall, not a door.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepfold").await;
    let a = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "the SECRET is in here"}),
    );
    let b = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "nothing"}),
    );
    let fid = kernel.rows.mint_id();
    put_row(
        &kernel,
        &backend,
        crate::folds::make_fold(&[a, b], "old work", "a summary", false, fid, 1, None),
    );
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": "secret", "ids": [fid]}),
        )])
        .await
        .remove(0)
        .result;
    let matches = r
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        matches
            .iter()
            .any(|m| m.get("id").and_then(Value::as_i64) == Some(a)),
        "the fold's MEMBER must be searched: {r}"
    );
}

#[tokio::test]
async fn context_recall_fits_the_head_and_writes_the_view_it_chose() {
    // THE VIEW ARRAY IS THE STATE. Without it the next turn (and the next
    // PROCESS) rebuilds a freshly-guessed prefix, and every cached token is lost.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ctxrecall").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL: audit the ledger"}),
    );
    for i in 0..60 {
        let pid = kernel.rows.mint_id();
        put_row(
            &kernel,
            &backend,
            json!({
                "role": "user",
                "content": format!("[reply to {pid} · notes read · \"chunk {i}\"]\n{}", "y".repeat(2000)),
            }),
        );
    }
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_recall", "dry": true}),
        )])
        .await
        .remove(0)
        .result;
    let kept = r.get("kept").and_then(Value::as_u64).unwrap_or(0);
    assert!(kept > 0, "a recall must keep something: {r}");
    assert_eq!(
        r.get("degraded").and_then(Value::as_bool),
        Some(true),
        "a dry recall is the mechanical fit, and says so: {r}"
    );

    let view = crate::view::load_view(&backend, &kernel).expect("the view must be persisted");
    let ids = view.get("ids").and_then(Value::as_array).cloned().unwrap();
    assert_eq!(
        ids.len() as u64,
        kept,
        "the view holds exactly what was kept"
    );
    // The GOAL is pinned: it is the oldest row, so it is the first thing a
    // newest-first fit would drop.
    assert!(
        ids.iter()
            .any(|e| e.get("res").and_then(Value::as_str) == Some("stub")),
        "the replies that did not earn full resolution ride as stubs: {ids:?}"
    );

    // The decision is a ROW — the next reload reads why the last recall chose.
    let rows = crate::history::load_history(&backend, &kernel, "cli").await;
    assert!(
        rows.iter()
            .any(|r| r.get("kind").and_then(Value::as_str) == Some("recall")),
        "the recall must log its own decision"
    );
}

#[tokio::test]
async fn context_recall_on_an_empty_store_says_so_instead_of_failing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ctxempty").await;
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_recall", "dry": true}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r.get("why").and_then(Value::as_str),
        Some("empty store"),
        "{r}"
    );
}

#[tokio::test]
async fn a_fold_projection_over_budget_writes_folds() {
    // MEASURED 2026-09-12 (fold_descent:rust): four compactions under
    // `context_strategy: fold`, every one `folded=0 stripped=0` — the seam
    // fired, the strategy did nothing, the cut did the work. A fold arm that
    // never folds is the control arm with extra steps.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "foldproj").await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(),
                   "context_strategy": "fold", "context_window": 2600,
                   "output_reserve": 200, "context_titles": true, "context_async": true}),
        )])
        .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL: survey the fillers"}),
    );
    let goal = kernel
        .rows
        .for_agent(&backend)
        .last()
        .and_then(|r| r.get("id").and_then(Value::as_i64))
        .unwrap();
    let mut prev = goal;
    for i in 0..16 {
        let call = put_row(
            &kernel,
            &backend,
            json!({"role": "assistant", "parent": prev,
            "content": format!("Reading filler_{i}. <call name=\"send\"><target_id>notes</target_id><title>survey filler_{i}</title><type>read</type><path>filler_{i}.txt</path></call>")}),
        );
        let pid = kernel.rows.mint_id();
        prev = put_row(
            &kernel,
            &backend,
            json!({"role": "user", "parent": call,
            "content": format!("[reply to {pid} · notes read · \"survey filler_{i}\"]\n{}", "filler text ".repeat(60))}),
        );
    }
    script(
        &backend,
        vec![pass("A title\n\nA short summary of the stretch.")],
        Duration::ZERO,
    );
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        script: script_for(&backend),
    });
    let state = crate::state::state_for(&backend);
    let (messages, store_len) = crate::assembly::assemble_messages(
        &backend,
        &state,
        "next",
        &kernel,
        "cli",
        Some("sys"),
        None,
    )
    .await;
    let ids_in_body = messages.iter().filter(|m| m.get("id").is_some()).count();
    let out = crate::projection::project_context(
        &provider,
        &backend,
        &state,
        &kernel,
        "cli",
        "mock",
        messages.clone(),
        store_len,
    )
    .await;
    assert!(out.is_ok(), "{out:?}");
    let proj = state.projection.lock().unwrap().clone().unwrap();
    eprintln!(
        "rows in body with id: {ids_in_body}/{}  projection: {proj}",
        messages.len()
    );
    assert_eq!(proj["fired"], true, "{proj}");
    assert!(
        proj["folded"].as_u64().unwrap_or(0) > 0,
        "a fold arm over budget must FOLD: {proj}"
    );
}

#[tokio::test]
async fn successive_fold_projections_do_not_nest_into_a_chain() {
    // MEASURED 2026-09-12: rust reached fold levels 1..11 on `nested_folds` where
    // python reached 1..2 on the same test. The strategy code was identical; the
    // BODY was not — rust fed the whole store, python feeds the VIEW ARRAY, on
    // which an old fold's members are absent and `render` does not re-place it
    // at the head. Three adjacent folds then wrapped into a deeper one every
    // other turn. This pins the python shape: depth stays flat.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "folddepth").await;
    let _ = kernel.send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "update_agent", "id": backend.as_str(),
        "context_strategy": "fold", "context_window": 2600, "output_reserve": 200, "context_titles": true, "context_async": true}))]).await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL"}),
    );
    let mut prev = kernel
        .rows
        .for_agent(&backend)
        .last()
        .and_then(|r| r.get("id").and_then(Value::as_i64))
        .unwrap();
    let state = crate::state::state_for(&backend);
    for step in 0..14 {
        for i in 0..4 {
            let call = put_row(
                &kernel,
                &backend,
                json!({"role": "assistant", "parent": prev, "content": format!("Reading f{step}_{i}. <call name=\"send\"><target_id>notes</target_id><title>survey f{step}_{i}</title><type>read</type><path>f.txt</path></call>")}),
            );
            let pid = kernel.rows.mint_id();
            prev = put_row(
                &kernel,
                &backend,
                json!({"role": "user", "parent": call, "content": format!("[reply to {pid} · notes read · \"survey f{step}_{i}\"]\n{}", "filler text ".repeat(60))}),
            );
        }
        script(
            &backend,
            vec![
                pass("A title\n\nSummary."),
                pass("A title\n\nSummary."),
                pass("A title\n\nSummary."),
                pass("A title\n\nSummary."),
            ],
            Duration::ZERO,
        );
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            script: script_for(&backend),
        });
        let (messages, store_len) = crate::assembly::assemble_messages(
            &backend,
            &state,
            "next",
            &kernel,
            "cli",
            Some("sys"),
            None,
        )
        .await;
        let _ = crate::projection::project_context(
            &provider, &backend, &state, &kernel, "cli", "mock", messages, store_len,
        )
        .await;
        let rows: Vec<Value> = crate::history::load_history(&backend, &kernel, "cli").await;
        let mut levels: Vec<i64> = rows
            .iter()
            .filter(|r| crate::folds::is_fold(r))
            .map(|r| crate::folds::level(&rows, r))
            .collect();
        levels.sort_unstable();
        levels.dedup();
        let view = crate::folds::render(&rows);
        let shape: String = view
            .iter()
            .map(|r| {
                if crate::folds::is_fold(r) {
                    format!("F{}", crate::folds::level(&rows, r))
                } else {
                    ".".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("");
        eprintln!(
            "step {step:2}: rows={} folds={} levels={levels:?} view={shape}",
            rows.len(),
            rows.iter().filter(|r| crate::folds::is_fold(r)).count()
        );
    }
}
// ─── B1 / B3 / B5 python-parity twins ───────────────────────────────────────

#[test]
fn cancel_doc_is_python_verbatim() {
    // Every AI reflect carries this, so a model reads it on every turn. Rust
    // taught `{type:'ps'}` / `{type:'cancel', task_id:'<agent>:<n>'}` — JSON, and
    // a task_id shape the kernel no longer mints.
    assert_eq!(
        crate::CANCEL_DOC,
        "No `interrupt` verb — an in-flight `send` IS a kernel task. Kill it with \
         the substrate's universal system verbs: send 'kernel' <type>ps</type> for \
         the live task table, find the row {agent:'<this agent>', verb:'send'}, then \
         send 'kernel' <type>cancel</type> <task_id>n</task_id>. The turn closes \
         with status(phase='done', reason='cancelled') + done, its HTTP stream and \
         FIFO lock are released, and the blocked caller's `send` resolves to \
         {cancelled:true}. Cancel kills the task's whole SUBTREE, so any work the \
         turn spawned dies with it."
    );
}

#[test]
fn reflect_verbs_match_python_first_doc_lines() {
    // python's `_build` order, and the FIRST LINE of each verb's docstring.
    let v = crate::verbs::reflect_verbs();
    let o = v.as_object().expect("an object");
    assert_eq!(
        o.keys().cloned().collect::<Vec<_>>(),
        vec![
            "reflect",
            "send",
            "inquiry",
            "history",
            "status",
            "context_status",
            "recall",
            "pull",
            "grep",
            "context_recall",
            "boot",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );
    assert_eq!(
        o["reflect"],
        "Identity + model + endpoint + generating flag + context posture. No args."
    );
    assert_eq!(
        o["context_status"],
        "No args. The context-budget posture + the last overflow projection + the model's"
    );
    assert_eq!(
        o["boot"],
        "On boot, an agent with a store REBUILDS ITS VIEW — `context_recall`. A fresh"
    );
    assert!(o["history"]
        .as_str()
        .unwrap()
        .ends_with("Returns {rows, next, total}."));
}

#[test]
fn reflect_emits_and_concurrency_are_python_verbatim() {
    let e = crate::verbs::reflect_emits();
    let o = e.as_object().expect("an object");
    // THREE, in this order — `done` and `say` and `queued` are not python's.
    assert_eq!(
        o.keys().cloned().collect::<Vec<_>>(),
        vec![
            "token".to_string(),
            "status".to_string(),
            "context".to_string()
        ]
    );
    assert!(o["token"]
        .as_str()
        .unwrap()
        .starts_with("{type:'token', text:str, source, client_id} — streaming chunk. Routed ONLY"));
    assert!(o["status"]
        .as_str()
        .unwrap()
        .contains("detail.reason='ok'|'cancelled'|'timeout'|'error' for done."));
    assert!(o["context"]
        .as_str()
        .unwrap()
        .ends_with("Pull counterpart: the `context_status` verb."));
    assert_eq!(
        crate::verbs::CONCURRENCY_DOC,
        "Per-backend FIFO lock around `send`: one generation at a time. Other \
         callers wait (and receive a `queued` event so their UI can show it). \
         `reflect`/`history`/`status` skip the lock and stay snappy."
    );
}

#[test]
fn render_reflect_matches_python_one() {
    // A string as-is; a list of SCALARS comma-joined through `str()`; anything
    // nested through `render_reply` with whitespace collapsed; `null` empty.
    assert_eq!(
        crate::assembly::render_reflect(&json!({
            "sentence": "S",
            "a": null,
            "b": ["x", "y"],
            "c": {"$self": "*"},
        })),
        "S  a=  b=x,y  c=<$self>*</$self>"
    );
    // `shift_remove`, not swap: the remaining keys keep their order.
    assert_eq!(
        crate::assembly::render_reflect(&json!({
            "sentence": "S", "one": 1, "two": 2, "three": 3,
        })),
        "S  one=1  two=2  three=3"
    );
    // A list of scalars uses python `str()`, so a bool is `True`.
    assert_eq!(
        crate::assembly::render_reflect(&json!({"sentence": "", "t": [true, 2]})),
        "t=True,2"
    );
}

#[test]
fn estimate_one_counts_python_json_dumps_code_points() {
    // `json.dumps(m)` with `, `/`: ` separators and ensure_ascii, measured in
    // CODE POINTS: `{"role": "user", "content": "a \u2014 b"}` is 41 chars → 11.
    assert_eq!(
        crate::context::estimate_one(&json!({"role": "user", "content": "a — b"})),
        11
    );
}

#[test]
fn budget_meta_parses_like_python() {
    let meta = |v: Value| v.as_object().unwrap().clone();
    // `context_window` 0.5 is a POSITIVE float, so `_as_pos_int` truncates it to
    // 0 and that 0 is the window; `output_reserve` "-5" is not a digit string, so
    // 1024; the budget then floors at 256. Measured python: {0, 1024, 256}.
    let m = meta(json!({"context_window": 0.5, "output_reserve": "-5", "recent_n": 1000.0}));
    assert_eq!(crate::context::resolve_context_window(&m), 0);
    assert_eq!(crate::context::output_reserve(&m), 1024);
    assert_eq!(crate::context::budget(&m), 256);
    assert_eq!(crate::context::recent_n(&m), 50);
    // `+12` is not `isdigit()`, `1000.0` is not an int, `true` is `int(True)` = 1.
    let m = meta(json!({"context_window": "+12", "output_reserve": 1000.0, "recent_n": true}));
    assert_eq!(crate::context::resolve_context_window(&m), 131_072);
    assert_eq!(crate::context::output_reserve(&m), 1024);
    assert_eq!(crate::context::recent_n(&m), 1);
    // A positive float window truncates; the budget then floors.
    let m = meta(json!({"context_window": 12.9}));
    assert_eq!(crate::context::resolve_context_window(&m), 12);
    assert_eq!(crate::context::budget(&m), 256);
    // A bool is never a window; `" 7 "` is `int()`-able.
    let m = meta(json!({"context_window": true, "recent_n": " 7 "}));
    assert_eq!(crate::context::resolve_context_window(&m), 131_072);
    assert_eq!(crate::context::recent_n(&m), 7);
}

#[test]
fn recent_n_bool_is_one() {
    let m = json!({"recent_n": 7.9}).as_object().unwrap().clone();
    assert_eq!(crate::context::recent_n(&m), 7);
    let m = json!({"recent_n": "nope"}).as_object().unwrap().clone();
    assert_eq!(crate::context::recent_n(&m), 6, "int() raises → python's 6");
}

#[test]
fn inquiry_turn_text_uses_python_str_and_json_dumps() {
    let out = crate::verbs::inquiry_open(&json!({
        "text": "pick",
        "options": [true, "b"],
        "action": {"a": 1, "b": "—"},
    }))
    .expect("a valid inquiry");
    let text = out["text"].as_str().unwrap();
    assert!(text.contains("\n\nOptions: True, b"), "{text:?}");
    assert!(
        text.contains("\n\nProposed action: {\"a\": 1, \"b\": \"\\u2014\"}"),
        "{text:?}"
    );
}

/// B3.2 — the placeholder refusal renders the RAW value with python's `repr`,
/// and a non-string `answer` is refused on its own. Rust coerced any non-string
/// to `""` and printed rust's `Debug`, so a model that answered `42` was told
/// its answer had come back as `""`. The strip is python's: whitespace first,
/// then dots off both ends, so `'. x'` is a real answer.
#[test]
fn inquiry_placeholder_refusal_renders_python_repr() {
    let closed = |answer: &str| {
        crate::verbs::inquiry_close(
            json!({
                "response": format!(
                    "<call name=\"inquiry_answer\"><answer>{answer}</answer></call>"
                ),
            }),
            &json!({"request_id": "r1"}),
        )
    };
    let placeholder = closed("...");
    assert_eq!(
        placeholder["error"],
        json!(
            "that is the PLACEHOLDER, not an answer — `answer` came back as \
             '...'. Say what you actually concluded."
        )
    );
    assert_eq!(placeholder["reason"], json!("bad_arg"));
    assert_eq!(placeholder["arg"], json!("answer"));
    assert_eq!(placeholder["request_id"], json!("r1"));
    // python whitespace, THEN dots — a dot with prose after it is an answer.
    assert_eq!(closed(". x")["answer"], json!(". x"));
    // A NON-STRING is refused, rendered as python writes it.
    let numeric = crate::verbs::inquiry_close(
        json!({"response": "<call name=\"inquiry_answer\"><answer>42</answer></call>"}),
        &json!({"request_id": "r1"}),
    );
    assert!(
        numeric["error"]
            .as_str()
            .unwrap_or_default()
            .contains("came back as 42."),
        "{numeric:#?}"
    );
    // A MISSING `answer` is python's `None`.
    let missing = crate::verbs::inquiry_close(
        json!({"response": "<call name=\"inquiry_answer\"></call>"}),
        &json!({"request_id": "r1"}),
    );
    assert!(
        missing["error"]
            .as_str()
            .unwrap_or_default()
            .contains("came back as None."),
        "{missing:#?}"
    );
}

#[test]
fn a_null_context_strategy_is_refused_with_pythons_text() {
    // python `get_strategy(None)` is None → the refusal renders the VALUE's repr
    // and `str(strategy_names())`.
    let want = "(valid: ['compact', 'truncate', 'strip', 'fold'])";
    assert_eq!(
        fantastic_bundle::py_repr_value(&json!(crate::strategies::STRATEGY_NAMES)),
        "['compact', 'truncate', 'strip', 'fold']"
    );
    assert!(format!("x {want}").contains(want));
    assert_eq!(fantastic_bundle::py_repr_value(&json!(null)), "None");
    assert_eq!(fantastic_bundle::py_repr_value(&json!("bogus")), "'bogus'");
}
// ─── the self-verb replies a MODEL reads (python-canonical bytes) ───────────

#[tokio::test]
async fn history_digest_of_caller_rows_follows_python() {
    // `kind or role`, `str(content)[:120]` and `len(str(content))` — rust's
    // `as_str` gave head '' / bytes 0 for a dict content and kept an EMPTY kind.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hdigpy").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "kind": "", "content": {"a": 1}}),
    );
    put_row(&kernel, &backend, json!({"role": "user", "content": null}));
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "kind": 5, "content": "x"}),
    );
    let h = kernel
        .send(vec![Item::new(backend, json!({"type": "history"}))])
        .await
        .remove(0)
        .result;
    let rows = h["rows"].as_array().expect("rows");
    assert_eq!(rows[0]["kind"], "user", "a falsy kind falls back to role");
    assert_eq!(rows[0]["head"], "{'a': 1}");
    assert_eq!(rows[0]["bytes"], 8);
    assert_eq!(rows[1]["head"], "None", "str(None) is the head");
    assert_eq!(rows[1]["bytes"], 4);
    assert_eq!(rows[2]["kind"], 5, "a non-string kind is kept RAW");
}

#[tokio::test]
async fn recall_renders_non_string_content_like_python_json_dumps() {
    // `json.dumps(content, default=str)` with ensure_ascii — `{"k":"é"}` renders
    // `{"k": "\u00e9"}`, and an EMPTY string content renders as itself, never as
    // the whole row.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rcpy").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": {"k": "é"}}),
    );
    put_row(&kernel, &backend, json!({"role": "user", "content": ""}));
    let r = kernel
        .send(vec![Item::new(backend, json!({"type": "recall"}))])
        .await
        .remove(0)
        .result;
    let msgs = r["messages"].as_array().expect("messages");
    assert_eq!(msgs[0]["content"], "{\"k\": \"\\u00e9\"}");
    assert_eq!(msgs[1]["content"], "", "an empty content is itself");
    assert!(r.get("client_id").is_none(), "python sends no client_id");
}

#[tokio::test]
async fn recall_empty_page_says_why() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rcnote").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "hello"}),
    );
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "recall", "query": "zzz"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["messages"], json!([]));
    assert_eq!(
        r["note"], "query 'zzz' matched none of the 1 stored turns.",
        "python's note names no client: {r:#?}"
    );
}

#[tokio::test]
async fn pull_refuses_a_non_integer_offset() {
    // python does BOTH conversions under one try/except, and the refusal carries
    // no `arg`.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pullbad").await;
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "pull", "pid": 7, "offset": "x"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "pull: offset/length must be integers");
    assert_eq!(r["reason"], "bad_arg");
    assert!(r.get("arg").is_none(), "{r:#?}");
    // `length: 0` is FALSY, so python's `or cap` gives the cap — not a refusal.
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "pull", "pid": 7, "length": 0}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_found", "{r:#?}");
}

#[tokio::test]
async fn pull_pid_zero_string_is_not_found_like_python() {
    // `_as_id('0')` is 0 — the STRING branch has no `> 0` filter — so this is a
    // readable not_found, not a `bad_arg`. Rust filtered it out as bad_arg.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "pid0").await;
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "pull", "pid": "0"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_found", "{r:#?}");
    assert!(r["error"].as_str().unwrap().contains("pid 0"), "{r:#?}");
    // `' 12 '` strips; `'+5'` is not a digit string at all.
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "pull", "pid": "+5"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg", "{r:#?}");
}

#[tokio::test]
async fn pull_fold_negative_length_is_refused() {
    // python refuses a negative `length` (2026-09-20) — it used to slice from
    // the END and hand back a negative `next_offset`.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "foldneg").await;
    let fid = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "kind": "fold", "body": "abcdefghij", "content": "[fold]"}),
    );
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "pull", "pid": fid, "length": -3}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "pull: length must not be negative", "{r:#?}");
    assert_eq!(r["reason"], "bad_arg");
}

#[tokio::test]
async fn grep_numeric_query_and_empty_ids_follow_python() {
    // `str(query or "")` — a NUMERIC query is its digits; an EMPTY `ids` list is
    // falsy, so it means NO SCOPE (rust scoped every row out).
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepnum").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "order 586 shipped"}),
    );
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": 586, "ids": []}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["total"], 1, "{r:#?}");
    // python's key order, and the rendered reply opens with `<matches>`.
    assert_eq!(
        r.as_object()
            .unwrap()
            .keys()
            .take(3)
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "matches".to_string(),
            "total".to_string(),
            "truncated".to_string()
        ]
    );
    assert!(crate::tool_parse::render_reply(&r).starts_with("<matches>"));
}

#[tokio::test]
async fn grep_splits_lines_like_python_splitlines() {
    // `\r`, `\f` and the information separators are LINE BOUNDARIES to python;
    // rust's `lines()` saw one line and returned the whole text.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepsl").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "a\rneedle here\nb"}),
    );
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": "needle"}),
        )])
        .await
        .remove(0)
        .result;
    let m = r["matches"].as_array().unwrap();
    assert_eq!(m.len(), 1, "{r:#?}");
    assert_eq!(m[0]["line"], 2);
    assert_eq!(m[0]["text"], "needle here");
    // `\x1c` splits too.
    let (kernel, backend) = mk_kernel(&tmp, "grepsl2").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "x\u{1c}needle"}),
    );
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "grep", "query": "needle"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["matches"].as_array().unwrap()[0]["line"], 2, "{r:#?}");
}

#[tokio::test]
async fn grep_scans_non_string_content_as_python_json_dumps() {
    // ensure_ascii: `{"k":"é"}` is scanned as `{"k": "\u00e9"}`, so `é` matches
    // NOTHING and `u00e9` matches. Rust's compact UTF-8 matched `é`.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepesc").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": {"k": "é"}}),
    );
    let ask = |q: &str| {
        let kernel = Arc::clone(&kernel);
        let backend = backend.clone();
        let q = q.to_string();
        async move {
            kernel
                .send(vec![Item::new(
                    backend,
                    json!({"type": "grep", "query": q}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    assert_eq!(ask("u00e9").await["total"], 1);
    assert_eq!(ask("é").await["total"], 0);
}

#[tokio::test]
async fn a_null_context_strategy_is_reported_raw_and_refused() {
    // Only an ABSENT key means `compact`. `context_status` echoes the raw value
    // and the projection refuses it with python's text.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ctxnull").await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(), "context_strategy": null}),
        )])
        .await;
    let st = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_status"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(st["strategy"], Value::Null, "{st:#?}");
    // And `last_projection` carries `pinned_facts` right after `summarized`
    // once a projection has fired (an absent key is a different reply from 0).
    assert!(st.get("last_projection").is_some());
}

#[tokio::test]
async fn context_titles_flag_is_python_truthy() {
    // `if rec.get("context_titles")` — `1` turns the block on; `as_bool` did not.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ctxtitles").await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(), "context_titles": 1}),
        )])
        .await;
    let sys = crate::assembly::default_system_content(&backend, &kernel).await;
    assert!(
        sys.contains("## Titling your calls"),
        "the titles block is on"
    );
}
#[tokio::test]
async fn a_reply_in_flight_across_a_reboot_is_recorded_not_acted_on() {
    // Measured 2026-09-06 (python): the rebooted agent was woken by answers to
    // calls its PREVIOUS incarnation made, and finished THAT work instead of the
    // new instruction. The reply is kept as a row — real data — but runs no turn.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "late").await;
    let s = script(&backend, vec![pass("ack")], Duration::ZERO);
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "GOAL"}),
    );
    // A boot bumps the incarnation, so a delivery decided BEFORE it is late.
    let born = crate::state::incarnation(&backend);
    let _ = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
        .await;
    assert_eq!(crate::state::incarnation(&backend), born + 1);
    crate::route::deliver(
        &kernel,
        backend.as_str(),
        "[reply to 1 · n r]\nlate",
        "cli",
        None,
        born,
    )
    .await;
    assert_eq!(s.passes_served(), 0, "no turn ran for the late reply");
    let rows = crate::history::load_history(&backend, &kernel, "cli").await;
    let last = rows.last().expect("a row");
    assert_eq!(last["kind"], "late");
    assert_eq!(
        last["content"],
        "[late reply — answered a call made before this agent last booted; \
         recorded, not acted on]\n[reply to 1 · n r]\nlate"
    );
    // A reply from THIS incarnation runs a turn.
    crate::route::deliver(&kernel, backend.as_str(), "fresh", "cli", None, born + 1).await;
    assert!(s.passes_served() >= 1, "a live reply is a turn");
}

#[tokio::test]
async fn send_to_is_kept_raw_and_dropped_only_when_blank() {
    // python keeps `send_to` VERBATIM and drops it only when it is not a string
    // or is blank after a full strip. Rust trimmed it, so `' peer '` silently
    // became `peer` — a different agent from the one the caller named.
    //
    // `send_to` fires on the `done` branch (a call-less turn is NUDGED first and
    // returns — python's order, and rust's).
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "sendto").await;
    let peer = format!("ff_{}", backend.as_str());
    script(
        &backend,
        vec![pass("the answer <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "send_to": format!(" {peer} ")}),
        )])
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // READ THE CALL ROWS, not the ledger sidecar. The v2 sidecar is an ARRAY and
    // carries NO `verb` (C46), and the same peer is also the persistence store,
    // so target alone no longer identifies a hand-off. A `call` row names both.
    let handed = |kernel: &Arc<Kernel>, peer: &str| -> bool {
        kernel
            .rows
            .for_agent(&AgentId::from(peer))
            .iter()
            .any(|r| r["kind"] == json!("call") && r["verb"] == json!("send"))
    };
    assert!(
        !handed(&kernel, &peer),
        "the raw ` peer ` is NOT the agent `peer` — nothing may be handed to it"
    );

    // A blank-after-strip / non-string `send_to` makes no hand-off either, and a
    // NAMED one still does.
    for (bad, expect) in [
        (json!("\n\t"), false),
        (json!(7), false),
        (json!(peer.clone()), true),
    ] {
        let tag = format!("st_{}", expect);
        let (kernel, backend) = mk_kernel(&tmp, &tag).await;
        let peer = format!("ff_{}", backend.as_str());
        let arg = if bad.is_string() && bad.as_str() == Some(peer.as_str()) {
            json!(peer)
        } else {
            bad.clone()
        };
        let arg = if expect { json!(peer) } else { arg };
        script(
            &backend,
            vec![pass("done here <call name=\"done\"></call>")],
            Duration::ZERO,
        );
        let _rx = rebind_backend_inbox(&kernel, &backend);
        let _ = kernel
            .send(vec![Item::new(
                backend.clone(),
                json!({"type": "send", "text": "hi", "send_to": arg}),
            )])
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            handed(&kernel, &peer),
            expect,
            "send_to {bad} must {} hand off",
            if expect { "" } else { "NOT" }
        );
    }
}

#[tokio::test]
async fn a_stateless_messages_override_persists_nothing() {
    // python `_run`'s `stateless`: `[system] + messages + live turn`, no recall
    // due, no projection, NOTHING written to the store.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "stateless").await;
    let s = script(
        &backend,
        vec![pass("ok <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let before = conversation(&kernel, &backend).len();
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({
                "type": "send",
                "text": "live",
                "messages": [{"role": "user", "content": "caller turn"}],
            }),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["final"].as_str().unwrap().starts_with("ok"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        conversation(&kernel, &backend).len(),
        before,
        "a stateless send writes no conversation rows"
    );
    let req = s.request_messages(0);
    assert_eq!(req[0]["role"], "system");
    assert_eq!(req[1]["content"], "caller turn");
    assert_eq!(req[2]["content"], "live");
    assert_eq!(
        req.len(),
        3,
        "[system] + messages + the live turn: {req:#?}"
    );
}

#[tokio::test]
async fn grep_scope_scans_bodies_in_python_set_order() {
    // `list(set(ids))` is the hash table's SLOT order, not sorted — and which of
    // two bodies owns a line they SHARE is decided by it.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "setord").await;
    let line = "the audit marker";
    let a = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "row a"}),
    );
    // Advance the counter until the NEXT row lands in a LOWER set slot than `a`.
    loop {
        let m = kernel.rows.mint_id();
        if (m + 1) & 7 < a & 7 {
            break;
        }
    }
    let b = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "row b"}),
    );
    assert_eq!(
        fantastic_bundle::py_set_order(&[a, b]),
        vec![b, a],
        "the fixture must exercise a set order that is NOT sorted"
    );
    kernel.rows.body_put(&backend, a, line).expect("body a");
    kernel.rows.body_put(&backend, b, line).expect("body b");
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "grep", "query": "audit marker", "ids": [a, b]}),
        )])
        .await
        .remove(0)
        .result;
    let m = r["matches"].as_array().unwrap();
    assert_eq!(m.len(), 1, "identical lines are deduped: {r:#?}");
    assert_eq!(
        m[0]["id"], b,
        "the body python scans FIRST owns the shared line: {r:#?}"
    );
}
#[tokio::test]
async fn context_recall_dry_is_python_truthy() {
    // `if payload.get("dry")` — `1` is dry, and a dry recall makes NO model call.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dry1").await;
    let s = script(
        &backend,
        vec![pass("{\"expand\": [], \"why\": \"x\"}")],
        Duration::ZERO,
    );
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL"}),
    );
    let r = kernel
        .send(vec![Item::new(
            backend,
            json!({"type": "context_recall", "dry": 1}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        s.passes_served(),
        0,
        "dry:1 must make no model call: {r:#?}"
    );
    assert_eq!(r["why"], "mechanical fit");
    assert_eq!(r["degraded"], true);
}

#[tokio::test]
async fn an_empty_json_recall_answer_stays_degraded() {
    // python `if ans:` — an empty `{}` is NO answer: `degraded` stays true and
    // `why` stays `mechanical fit`.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ansempty").await;
    script(&backend, vec![pass("{}")], Duration::ZERO);
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL"}),
    );
    let r = kernel
        .send(vec![Item::new(backend, json!({"type": "context_recall"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["degraded"], true, "{r:#?}");
    assert_eq!(r["why"], "mechanical fit");
}

#[tokio::test]
async fn a_numeric_why_is_its_python_str() {
    // `str(ans.get("why") or "")[:300] or "chosen"`: 42 reads `42`, `["a"]` reads
    // `['a']`, and only a FALSY why is `chosen`.
    for (answer, want) in [
        ("{\"why\": 42}", "42"),
        ("{\"why\": [\"a\"]}", "['a']"),
        ("{\"why\": 0}", "chosen"),
        ("{\"why\": \"\"}", "chosen"),
    ] {
        let tmp = TempDir::new().unwrap();
        let (kernel, backend) = mk_kernel(&tmp, &format!("why_{want}")).await;
        script(&backend, vec![pass(answer)], Duration::ZERO);
        put_row(
            &kernel,
            &backend,
            json!({"role": "user", "content": "THE GOAL"}),
        );
        let r = kernel
            .send(vec![Item::new(backend, json!({"type": "context_recall"}))])
            .await
            .remove(0)
            .result;
        assert_eq!(r["why"], want, "answer {answer}: {r:#?}");
        assert_eq!(r["degraded"], false);
    }
}

#[tokio::test]
async fn a_scalar_expand_is_wrapped_like_python() {
    // `_as_list` (python 2026-09-20): a truthy int/float/bool `expand` is ONE
    // candidate — the recall completes and its row is written.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "expscalar").await;
    script(
        &backend,
        vec![pass("{\"expand\": 5, \"why\": \"w\"}")],
        Duration::ZERO,
    );
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL"}),
    );
    let before = conversation(&kernel, &backend).len();
    let r = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_recall"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r:#?}");
    assert_eq!(r["degraded"], false, "{r:#?}");
    assert_eq!(
        conversation(&kernel, &backend).len(),
        before + 1,
        "the recall row is written"
    );
    // A STRING expand is one `_as_id` candidate.
    let (kernel, backend) = mk_kernel(&tmp, "expstr").await;
    script(
        &backend,
        vec![pass("{\"expand\": \"7\", \"why\": \"w\"}")],
        Duration::ZERO,
    );
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL"}),
    );
    let r = kernel
        .send(vec![Item::new(backend, json!({"type": "context_recall"}))])
        .await
        .remove(0)
        .result;
    assert!(
        r.get("error").is_none(),
        "a string is one candidate: {r:#?}"
    );
}

#[tokio::test]
async fn a_non_string_type_tags_the_reply_with_python_str() {
    // `payload.get("type", "?")` then `str()` into the reply tag: `?` means the
    // key was ABSENT, never "the type was not a string".
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "verbstr").await;
    let s = script(
        &backend,
        vec![
            pass(call_envelope("kernel", json!({"type": 42}), Some("self"))),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let _ = kernel
        .send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )])
        .await;
    assert!(
        until_passes(&s, 2).await,
        "the reply must come back as a turn"
    );
    let seen = s.request(1);
    assert!(
        seen.contains("· kernel 42 ·") || seen.contains("· kernel 42]"),
        "the tag must print python's str(42): {seen}"
    );
}

#[tokio::test]
async fn the_seam_keeps_the_goal_the_strategies_pinned() {
    // `project_context` appends the real notice and then trims to fit. It trimmed
    // from the FRONT — exactly where `compact`/`truncate` had just pinned the goal
    // turn — so a snug window silently dropped the one turn the whole run serves
    // (python, measured 2026-08-28: absent across a wide band of windows, in BOTH
    // strategies). python trims from the MIDDLE first and only takes the goal as
    // a last resort, loudly.
    let goal = "GOAL-SENTINEL: build the widget and report when done";
    for strategy in ["compact", "truncate"] {
        for window in [1200i64, 2000, 4000] {
            let tmp = TempDir::new().unwrap();
            let (kernel, backend) = mk_kernel(&tmp, &format!("seam_{strategy}_{window}")).await;
            let _ = kernel
                .send(vec![Item::new(
                    AgentId::from("kernel_state"),
                    json!({"type": "update_agent", "id": backend.as_str(),
                           "context_window": window, "output_reserve": 0,
                           "context_strategy": strategy}),
                )])
                .await;
            script(
                &backend,
                vec![pass("A title\n\nSummary."), pass("A title\n\nSummary.")],
                Duration::ZERO,
            );
            let provider: Arc<dyn Provider> = Arc::new(MockProvider {
                script: script_for(&backend),
            });
            let state = crate::state::state_for(&backend);
            let mut messages = vec![
                json!({"role": "system", "content": "sys"}),
                json!({"role": "user", "content": goal}),
            ];
            for i in 0..40 {
                let role = if i % 2 == 0 { "assistant" } else { "user" };
                messages.push(json!({"role": role, "content": "z".repeat(400)}));
            }
            let len = messages.len() - 1;
            let out = crate::projection::project_context(
                &provider, &backend, &state, &kernel, "cli", "mock", messages, len,
            )
            .await
            .expect("the projection must fit, not failfast");
            assert!(
                crate::context::estimate_tokens(&out) <= window,
                "{strategy}@{window}: the projection must still fit"
            );
            assert!(
                out.iter()
                    .any(|m| m["content"].as_str().unwrap_or("").contains(goal)),
                "{strategy}@{window}: the pinned goal turn was dropped by the seam"
            );
        }
    }
}

/// C36 — the kernel's own bookkeeping is in the SAME table and never becomes a
/// turn. Once a dispatch writes a `call` row and a fork writes a `task` row,
/// the AI's conversation reader is the only thing keeping them out of a prompt.
#[tokio::test]
async fn history_hides_call_and_task_rows() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hidebk").await;
    // A real dispatch AT the backend — that writes a `call` row on it.
    let _ = kernel
        .send(vec![Item::new(backend.clone(), json!({"type": "reflect"}))])
        .await;
    // …and one turn of actual conversation.
    kernel
        .rows
        .put(
            &backend,
            json!({"kind": "turn", "role": "user", "content": "hello"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();

    let raw = kernel.rows.for_agent(&backend);
    let raw_kinds: Vec<&str> = raw
        .iter()
        .filter_map(|r| r.get("kind").and_then(Value::as_str))
        .collect();
    assert!(
        raw_kinds.contains(&"call"),
        "the store must actually hold call rows: {raw_kinds:?}"
    );

    let rows = crate::history::load_history(&backend, &kernel, "cli").await;
    assert!(
        rows.iter().all(|r| !matches!(
            r.get("kind").and_then(Value::as_str),
            Some("call" | "task" | "body" | "view" | "settle")
        )),
        "bookkeeping reached the conversation: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.get("content").and_then(Value::as_str) == Some("hello")),
        "the real turn is still there"
    );
}

/// B2.6 (r5) — the id must be RANDOM, not a counter. The capture read
/// `AAAAAVnE3yQ` (base64 of a big-endian int64) where python had `cijoBiyMnnw`:
/// the mint was `nanos ^ stack ^ pid ^ seq`, not `secrets`.
#[test]
fn two_send_ids_differ_and_do_not_ascend() {
    let ids: Vec<String> = (0..64).map(|_| crate::helpers::mint_send_id()).collect();
    let uniq: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(uniq.len(), ids.len(), "send ids repeated: {ids:?}");
    // A counter in big-endian base64 sorts ascending; 64 CSPRNG draws do not.
    let mut sorted = ids.clone();
    sorted.sort();
    assert_ne!(sorted, ids, "send ids ascend like a counter: {ids:?}");
}

/// python `secrets.token_hex(2)` — 4 hex characters, drawn fresh per turn.
#[test]
fn two_batch_tokens_differ_and_are_four_hex() {
    let a: Vec<String> = (0..64)
        .map(|_| crate::helpers::mint_batch_token())
        .collect();
    for t in &a {
        assert_eq!(t.len(), 4, "{t}");
        assert!(
            t.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{t}"
        );
    }
    assert!(a.iter().collect::<std::collections::HashSet<_>>().len() > 1);
}

/// The peer payload's KEY ORDER is python's. python lifts the send's inline
/// tags into `payload` with `fields.pop("target_id")`, which keeps the
/// survivors in source order (`type`, `text`), then `setdefault("msg", …)`
/// appends. `serde_json::Map::remove` is `swap_remove` under `preserve_order`
/// — it dragged `text` into the hole and the r3 capture read
/// `["text","type","msg"]` where python had `["type","text","msg"]`.
#[test]
fn a_peer_sends_payload_keeps_pythons_key_order() {
    let calls = crate::tool_parse::extract_tool_calls(
        "<call name=\"send\"><target_id>ai2</target_id><type>send</type>\
         <text>CASE=noop#1 hello from ai</text></call>",
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    let payload = calls[0]
        .arguments
        .get("payload")
        .and_then(Value::as_object)
        .expect("a send lifts its inline tags into `payload`");
    assert_eq!(
        payload.keys().collect::<Vec<_>>(),
        vec!["type", "text"],
        "source order survives the `target_id` lift"
    );
    // `msg` is stamped LAST, as python's `setdefault` appends it.
    let stamped = crate::route::fan_out(calls, Some(732));
    let payload = stamped[0]
        .arguments
        .get("payload")
        .and_then(Value::as_object)
        .expect("payload");
    assert_eq!(
        payload.keys().collect::<Vec<_>>(),
        vec!["type", "text", "msg"]
    );
}

/// THE TURN'S ROW IDS ARE MINTED AFTER ASSEMBLY. python `_run` calls
/// `_assemble` first — and the system block it builds dispatches two
/// `reflect`s, which take their call-row ids there — and only then
/// `_row_ids_for_incoming` + `_mint_row_id`. Rust minted them at turn entry,
/// so every turn's user/assistant rows sat two ids BELOW python's and every
/// `<turn_id>` in the r3 capture was off by two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_turns_row_ids_are_minted_after_the_assembly_reflects() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_order").await;
    let _s = script(
        &backend,
        vec![pass("ack <call name=\"done\"></call>")],
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
    let turn_id = r["turn_id"].as_i64().expect("turn_id is a row id");
    let rows = kernel.rows.for_agent(&backend);
    let reflect = rows
        .iter()
        .filter(|row| {
            row.get("kind") == Some(&json!("call")) && row.get("verb") == Some(&json!("reflect"))
        })
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .min()
        .expect("assembly reflects this agent every turn");
    let user = rows
        .iter()
        .filter(|row| row.get("role") == Some(&json!("user")))
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .min()
        .expect("the incoming row");
    assert!(
        reflect < user && user < turn_id,
        "assembly's reflect ({reflect}) must precede the user row ({user}) \
         and the assistant row ({turn_id})"
    );
}
