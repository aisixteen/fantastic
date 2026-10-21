//! STREAMING PARTS: a model's
//! output streams to its addressees as parts — animation, never action; a
//! receiver can stop it; a cut pass leaves a marker on top of the context, never a
//! half answer. Case for case the twin of python
//! `ai_core/tests/test_stream_parts.py`.
//!
//! A child of `tests`, so it drives the same harness (a real `ram_store` named
//! `memory` under the root); the provider here streams each script entry in
//! 4-char chunks, optionally slowly — python's `_Slow`.

use super::*;
use crate::stream_parts::{cut_marker, Part, PartEvent, PartKind, PartSplitter};
use fantastic_kernel::test_support::{as_agent, wire};
use fantastic_kernel::Status;
use futures_util::StreamExt;
use std::collections::HashMap;

const CALL: &str = concat!(
    "<call name=\"send\">\n<target_id>notes</target_id>\n<type>reflect</type>\n",
    "<bytes>a </call> inside</bytes>\n</call>"
);
const DONE: &str = "<call name=\"done\"></call>";

// ─── the splitter ────────────────────────────────────────────────────────────

fn run(text: &str, n: usize) -> (Vec<PartEvent>, PartSplitter) {
    let mut sp = PartSplitter::new("ai");
    let mut ev = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(n) {
        ev.extend(sp.feed(&chunk.iter().collect::<String>()));
    }
    ev.extend(sp.finish());
    (ev, sp)
}

/// `(event, kind)` for every non-delta event.
fn shape(ev: &[PartEvent]) -> Vec<(&'static str, &'static str)> {
    ev.iter()
        .filter_map(|e| match e {
            PartEvent::Start(p) => Some(("start", p.kind.as_str())),
            PartEvent::End(p, _) => Some(("end", p.kind.as_str())),
            PartEvent::Abort(p, _) => Some(("abort", p.kind.as_str())),
            PartEvent::Delta(..) => None,
        })
        .collect()
}

#[test]
fn parts_are_the_same_at_every_chunk_size() {
    let text = format!("hi <think>plan</think>ok {CALL} after {DONE}");
    for n in [1, 2, 5, text.chars().count()] {
        let (ev, sp) = run(&text, n);
        assert_eq!(
            shape(&ev),
            vec![
                ("start", "text"),
                ("end", "text"),
                ("start", "thinking"),
                ("end", "thinking"),
                ("start", "text"),
                ("end", "text"),
                ("start", "call"),
                ("end", "call"),
                ("start", "text"),
                ("end", "text"),
                ("start", "call"),
                ("end", "call"),
            ],
            "{n}"
        );
        let call = &sp.parts[3];
        assert_eq!(call.name.as_deref(), Some("send"));
        assert_eq!(call.target.as_deref(), Some("notes"));
        let body: String = ev
            .iter()
            .filter_map(|e| match e {
                PartEvent::Delta(p, t) if p.stream_id == call.stream_id => Some(t.as_str()),
                _ => None,
            })
            .collect();
        // verbatim bytes never close the call
        assert!(body.contains("a </call> inside"), "{n}: {body}");
    }
}

#[test]
fn an_open_call_when_the_pass_ends_aborts() {
    let (ev, _) = run("<call name=\"send\"><target_id>x</target_id><type>wr", 3);
    assert!(shape(&ev).contains(&("abort", "call")), "{ev:?}");
}

/// ONLY THE ANSWER ACTS: a `<call>` inside `<think>` streams as
/// thinking — no call part opens. Python `test_a_call_inside_a_thought_is_thinking_text`.
#[test]
fn a_call_inside_a_thought_is_thinking_text() {
    let (ev, _) = run(&format!("<think>let me {DONE} then</think>"), 4);
    assert_eq!(shape(&ev), vec![("start", "thinking"), ("end", "thinking")]);
}

#[test]
fn the_cut_marker_is_a_marker_not_a_half_answer() {
    let unsent = Part {
        stream_id: String::new(),
        kind: PartKind::Call,
        name: Some("send".into()),
        target: Some("x".into()),
        to: None,
    };
    let m = cut_marker(
        "said this <think>hidden</think><call name=\"send\"><target_id>x</target_id>",
        "model stopped generating",
        &[unsent],
    );
    assert_eq!(
        m.lines().collect::<Vec<_>>(),
        vec![
            "[model stopped generating]",
            "said this",
            "[a send call to x was not sent]",
        ]
    );
    assert_eq!(
        cut_marker("<think>hmm", "stopped by rx", &[]),
        "[stopped by rx while thinking]"
    );
}

// ─── through an AI agent ─────────────────────────────────────────────────────

const SLOW: &str = "slow_backend.tools";

/// One agent's scripted passes, each streamed chunk by chunk — python's `_Slow`.
struct SlowScript {
    scripts: std::sync::Mutex<std::collections::VecDeque<String>>,
    step: std::sync::Mutex<Duration>,
    requests: std::sync::Mutex<Vec<Vec<Value>>>,
}

impl SlowScript {
    fn requests(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn request(&self, n: usize) -> Vec<Value> {
        self.requests.lock().unwrap()[n].clone()
    }
}

static SLOW_SCRIPTS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<SlowScript>>>> =
    std::sync::OnceLock::new();

fn slow_scripts() -> std::sync::MutexGuard<'static, HashMap<String, Arc<SlowScript>>> {
    SLOW_SCRIPTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
}

fn slow(agent: &AgentId, scripts: &[&str], step: Duration) -> Arc<SlowScript> {
    let s = Arc::new(SlowScript {
        scripts: std::sync::Mutex::new(scripts.iter().map(|t| (*t).to_string()).collect()),
        step: std::sync::Mutex::new(step),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    slow_scripts().insert(agent.as_str().to_string(), Arc::clone(&s));
    s
}

struct SlowProvider(Arc<SlowScript>);

#[async_trait]
impl Provider for SlowProvider {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.0.requests.lock().unwrap().push(messages.to_vec());
        let text = self
            .0
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| DONE.to_string());
        let step = *self.0.step.lock().unwrap();
        let chars: Vec<char> = text.chars().collect();
        let chunks: Vec<String> = chars.chunks(4).map(|c| c.iter().collect()).collect();
        Ok(Box::pin(futures_util::stream::iter(chunks).then(
            move |c| async move {
                if !step.is_zero() {
                    tokio::time::sleep(step).await;
                }
                Ok(ProviderEvent::Token(c))
            },
        )))
    }
    fn model(&self) -> String {
        "stub".into()
    }
}

struct SlowBundle;

#[async_trait]
impl Bundle for SlowBundle {
    fn name(&self) -> &str {
        "slow_backend"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        // An unscripted agent (its `boot` at create) serves DONE passes.
        let script = Arc::clone(
            slow_scripts()
                .entry(agent_id.as_str().to_string())
                .or_insert_with(|| {
                    Arc::new(SlowScript {
                        scripts: std::sync::Mutex::new(std::collections::VecDeque::new()),
                        step: std::sync::Mutex::new(Duration::ZERO),
                        requests: std::sync::Mutex::new(Vec::new()),
                    })
                }),
        );
        let reply = verbs::handle_common(agent_id, payload, kernel, CFG, move |_client| {
            Arc::new(SlowProvider(Arc::clone(&script))) as Arc<dyn Provider>
        })
        .await?
        .unwrap_or_else(|| json!({"error": "slow_backend: unknown type"}));
        Ok(Some(reply))
    }
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &fantastic_kernel::TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await
    }
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        verbs::on_delete(agent_id, kernel).await;
        Ok(())
    }
}

async fn slow_kernel(tmp: &TempDir) -> Arc<Kernel> {
    base_kernel(tmp, |k| k.bundles.register(SLOW, SlowBundle)).await
}

/// python `_ai`: an AI agent on the shared memory store, granted `cli`.
async fn make_ai(kernel: &Arc<Kernel>, id: &str) -> AgentId {
    make_ai_as(kernel, id, SLOW).await
}

/// [`make_ai`] over the bundle `module` names.
async fn make_ai_as(kernel: &Arc<Kernel>, id: &str, module: &str) -> AgentId {
    let memory =
        fantastic_ram_store::test_support::memory_meta(kernel, id, json!({"cli": "*"})).await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": module,
        "id": id,
        "model": "stub",
        "context_window": 32768,
    });
    for (k, v) in memory {
        create[k] = v;
    }
    let r = call(kernel, "kernel_state", create).await;
    assert_eq!(r["id"], json!(id), "{r}");
    AgentId::from(id)
}

async fn ram_store(kernel: &Arc<Kernel>, id: &str) {
    let r = call(
        kernel,
        "kernel_state",
        json!({
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": fantastic_ram_store::HANDLER_MODULE,
            "id": id,
        }),
    )
    .await;
    assert_eq!(r["id"], json!(id), "{r}");
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// A registered reader of `src`'s traffic.
async fn watcher(
    kernel: &Arc<Kernel>,
    src: &str,
    id: &str,
) -> fantastic_kernel::inbox::InboxReceiver {
    let rx = kernel.ensure_inbox(&AgentId::from(id));
    assert!(kernel
        .watch(&AgentId::from(src), AgentId::from(id))
        .await
        .is_none());
    rx
}

/// Every `part_*` event queued on the reader so far.
fn drain_parts(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(v) = rx.try_recv() {
        if v.payload
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("part_"))
        {
            out.push(v.payload);
        }
    }
    out
}

fn types(evs: &[Value]) -> Vec<String> {
    evs.iter()
        .map(|e| e["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

async fn until_requests(s: &Arc<SlowScript>, n: usize) {
    for _ in 0..250 {
        if s.requests() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out: {} of {n} requests", s.requests());
}

/// The last assistant message handed to the model in pass `n`.
fn last_assistant(s: &Arc<SlowScript>, n: usize) -> String {
    s.request(n)
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .and_then(|m| m["content"].as_str())
        .expect("an assistant message")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_part_streams_to_its_addressee() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_ai")).await;
    ram_store(&kernel, "notes").await;
    wire(&kernel, ai.as_str(), "notes", json!("*"));
    let mut rx = watcher(&kernel, "notes", "w_notes").await;
    let fp = slow(&ai, &[CALL, DONE], Duration::ZERO);
    call(&kernel, ai.as_str(), json!({"type": "send", "text": "go"})).await;
    until_requests(&fp, 2).await;
    let evs = drain_parts(&mut rx);
    let t = types(&evs);
    assert_eq!(t.first().map(String::as_str), Some("part_start"), "{t:?}");
    assert_eq!(t.last().map(String::as_str), Some("part_end"), "{t:?}");
    assert_eq!(evs[0]["name"], json!("send"));
    assert_eq!(evs[0]["target"], json!("notes"));
    let sids: std::collections::HashSet<String> = evs
        .iter()
        .map(|e| e["stream_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sids.len(), 1, "{evs:?}");
    let body: String = evs
        .iter()
        .filter(|e| e["type"] == "part_delta")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert!(body.contains("a </call> inside"), "{body}");
}

/// The model stops generating mid-call: the call is NOT dispatched, and the
/// conversation's newest assistant row is the cut marker, which the next turn sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_that_stops_with_a_block_open_leaves_the_marker_on_top() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_cut")).await;
    ram_store(&kernel, "notes").await;
    wire(&kernel, ai.as_str(), "notes", json!("*"));
    let mut rx = watcher(&kernel, "notes", "w_cut").await;
    let open_call = "<call name=\"send\"><target_id>notes</target_id><type>refl";
    let fp = slow(&ai, &[open_call, DONE], Duration::ZERO);
    call(&kernel, ai.as_str(), json!({"type": "send", "text": "go"})).await;
    until_requests(&fp, 2).await;
    let marker = last_assistant(&fp, 1);
    assert_eq!(
        marker.lines().collect::<Vec<_>>(),
        vec![
            "[model stopped generating]",
            "[a send call to notes was not sent]",
        ],
        "{marker}"
    );
    let seen = types(&drain_parts(&mut rx));
    assert_eq!(
        seen.first().map(String::as_str),
        Some("part_start"),
        "{seen:?}"
    );
    assert_eq!(
        seen.last().map(String::as_str),
        Some("part_abort"),
        "{seen:?}"
    );
    // nothing was dispatched to notes: no reply came back for it
    let nxt = fp.request(1);
    let last = nxt.last().unwrap()["content"].to_string();
    assert!(!last.contains("reflect"), "{last}");
}

/// A stream that never sends a byte is stopped like any other: `stop` closes the
/// turn, and `status` reports `generating: false` straight after (#177).
#[tokio::test]
async fn stop_during_a_stalled_stream_clears_generating() {
    // the send ceiling is process-wide: hold the lock a ceiling test takes, so this
    // turn runs under the default ceiling
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_stall")).await;
    ram_store(&kernel, "rx").await;
    wire(&kernel, "rx", ai.as_str(), json!("*"));
    // the first chunk never comes: the stream is stalled until the stop
    let fp = slow(&ai, &["never"], Duration::from_secs(600));
    let k = Arc::clone(&kernel);
    let target = ai.clone();
    let turn = tokio::spawn(async move {
        k.send(vec![Item::new(
            target,
            json!({"type": "send", "text": "go", "client_id": "tab"}),
        )])
        .await
        .remove(0)
    });
    until_requests(&fp, 1).await;
    let before = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "status"}))]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(before["generating"], json!(true), "{before}");
    let out = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "stop"}))]),
    )
    .await
    .remove(0);
    assert_eq!(out.result, json!({"stopped": true}), "{:?}", out.result);
    let after = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "status"}))]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(after["generating"], json!(false), "{after}");
    assert_eq!(turn.await.unwrap().status, Status::Cancelled);
    // the aborted marker: the turn keeps its message and says who stopped it
    let rows = conversation(&kernel, &ai).await;
    let marker = rows
        .iter()
        .find(|r| r["role"] == json!("assistant"))
        .and_then(|r| r["content"].as_str())
        .unwrap_or_default();
    assert!(marker.contains("[stopped by rx"), "{rows:#?}");
    assert!(
        rows.iter().any(|r| r["content"] == json!("go")),
        "{rows:#?}"
    );
}

/// The SAME provider, behind a bundle whose cancel hook does not close the turn —
/// the kernel's cancel walk reaches the task and kills it, but the turn is left
/// open for `stop` to close (#177).
const SLOW_DEAF: &str = "slow_backend_deaf.tools";

struct DeafBundle;

#[async_trait]
impl Bundle for DeafBundle {
    fn name(&self) -> &str {
        "slow_backend_deaf"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        SlowBundle.handle(agent_id, payload, kernel).await
    }
}

/// `stop` reaches a stream the kernel's cancel did not close (#177): the turn is
/// closed by `stop` itself — `generating` goes false, the aborted marker is stored,
/// and the stream is gone, so the FIFO lock is free for the next send at once.
#[tokio::test]
async fn stop_closes_a_stalled_turn_the_kernel_cancel_did_not_reach() {
    // the send ceiling is process-wide: hold the lock a ceiling test takes, so this
    // turn runs under the default ceiling
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| k.bundles.register(SLOW_DEAF, DeafBundle)).await;
    let ai = make_ai_as(&kernel, &id_for(&tmp, "sp_deaf"), SLOW_DEAF).await;
    ram_store(&kernel, "rx").await;
    wire(&kernel, "rx", ai.as_str(), json!("*"));
    let fp = slow(&ai, &["never"], Duration::from_secs(600));
    let k = Arc::clone(&kernel);
    let target = ai.clone();
    let turn = tokio::spawn(async move {
        k.send(vec![Item::new(
            target,
            json!({"type": "send", "text": "go", "client_id": "tab"}),
        )])
        .await
        .remove(0)
    });
    until_requests(&fp, 1).await;
    let out = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "stop"}))]),
    )
    .await
    .remove(0);
    assert_eq!(out.result, json!({"stopped": true}), "{:?}", out.result);
    let after = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "status"}))]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(after["generating"], json!(false), "{after}");
    let rows = conversation(&kernel, &ai).await;
    assert!(
        rows.iter().any(|r| r["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("[stopped by rx"))),
        "{rows:#?}"
    );
    // the stream is gone: the next send runs at once, not behind the 600s stall
    *fp.step.lock().unwrap() = Duration::ZERO;
    let next = as_agent(
        "rx",
        kernel.send(vec![Item::new(
            ai.clone(),
            json!({"type": "send", "text": "after", "client_id": "tab"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert!(next.get("raised").is_none(), "{next}");
    assert_eq!(fp.requests(), 2, "the next send ran its own pass");
    let _ = turn.await;
}

/// A turn woken by a tool reply (#177) — the shape `deliver_reply` gives it: the
/// send runs as a dispatch row nested under a `deliver_reply` task. Stalled, then
/// stopped. The kernel's cancel reaches it: `stop` closes the turn through the
/// walk's `on_cancel` and needs no fallback. The turn ends, the marker is stored,
/// `status` reads `generating: false`, and the next send runs at once.
#[tokio::test]
async fn stop_reaches_a_reply_woken_turn() {
    // the send ceiling is process-wide: hold the lock a ceiling test takes, so this
    // turn runs under the default ceiling
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_reply")).await;
    ram_store(&kernel, "rx").await;
    wire(&kernel, "rx", ai.as_str(), json!("*"));
    let fp = slow(&ai, &["never"], Duration::from_secs(600));
    let k = Arc::clone(&kernel);
    let target = ai.clone();
    let born = crate::state::incarnation(&ai);
    kernel.spawn_task(&ai, "deliver_reply", async move {
        crate::route::deliver(&k, target.as_str(), "tool reply", "tab", None, born, false).await;
    });
    until_requests(&fp, 1).await;
    let out = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "stop"}))]),
    )
    .await
    .remove(0);
    assert_eq!(out.result, json!({"stopped": true}), "{:?}", out.result);
    let after = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "status"}))]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(after["generating"], json!(false), "{after}");
    let rows = conversation(&kernel, &ai).await;
    assert!(
        rows.iter().any(|r| r["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("[stopped by rx"))),
        "{rows:#?}"
    );
    *fp.step.lock().unwrap() = Duration::ZERO;
    let next = as_agent(
        "rx",
        kernel.send(vec![Item::new(
            ai.clone(),
            json!({"type": "send", "text": "after", "client_id": "tab"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert!(next.get("raised").is_none(), "{next}");
    assert_eq!(fp.requests(), 2, "the next send ran its own pass");
}

/// THE ABORT ARM (#177). The kernel's cancel does NOT find the turn: the task id
/// its in-flight entry records is not in the task table, so `cancel_task` answers
/// `{cancelled:false}` and runs no `on_cancel`. `stop` then closes the turn itself,
/// fires the generation's abort, and the send returns `{cancelled:true}` from its
/// abort arm — its provider stream dropped, the FIFO lock released — so the next
/// send runs at once.
#[tokio::test]
async fn stop_aborts_a_send_the_kernel_cancel_does_not_find() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_abort_arm")).await;
    ram_store(&kernel, "rx").await;
    wire(&kernel, "rx", ai.as_str(), json!("*"));
    let fp = slow(&ai, &["never"], Duration::from_secs(600));
    let k = Arc::clone(&kernel);
    let target = ai.clone();
    let turn = tokio::spawn(async move {
        k.send(vec![Item::new(
            target,
            json!({"type": "send", "text": "go", "client_id": "tab"}),
        )])
        .await
        .remove(0)
    });
    until_requests(&fp, 1).await;
    // a task id the kernel's table never held: the kernel cancel cannot find it
    const UNKNOWN_TASK: &str = "9000000001";
    {
        let state = crate::state::state_for(&ai);
        let mut cur = state.current_meta.lock().expect("current poisoned");
        cur.as_mut().expect("a turn in flight").task_id = Some(UNKNOWN_TASK.to_string());
    }
    let out = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "stop"}))]),
    )
    .await
    .remove(0);
    assert_eq!(out.result, json!({"stopped": true}), "{:?}", out.result);
    // the send's abort arm returned: a cancelled answer, not a stream left open
    let turn_out = turn.await.expect("the turn task");
    assert_eq!(
        turn_out.result["cancelled"],
        json!(true),
        "{:?}",
        turn_out.result
    );
    let after = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "status"}))]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(after["generating"], json!(false), "{after}");
    let rows = conversation(&kernel, &ai).await;
    assert!(
        rows.iter().any(|r| r["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("[stopped by rx"))),
        "{rows:#?}"
    );
    *fp.step.lock().unwrap() = Duration::ZERO;
    let next = as_agent(
        "rx",
        kernel.send(vec![Item::new(
            ai.clone(),
            json!({"type": "send", "text": "after", "client_id": "tab"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert!(next.get("raised").is_none(), "{next}");
    assert_eq!(fp.requests(), 2, "the next send ran its own pass");
}

/// One thread, as python's event loop: the stop lands between two chunks.
#[tokio::test]
async fn a_receiver_stops_the_stream_and_the_turn_keeps_a_marker() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_stop")).await;
    ram_store(&kernel, "rx").await;
    wire(&kernel, "rx", ai.as_str(), json!("*"));
    let mut w = watcher(&kernel, ai.as_str(), "w_stop").await;
    let long = "partial words ".repeat(200);
    let fp = slow(&ai, &[&long, DONE], Duration::from_millis(10));
    // the send dispatch lasts as long as the stream: run it, stop it mid-way
    let k = Arc::clone(&kernel);
    let target = ai.clone();
    let turn = tokio::spawn(async move {
        k.send(vec![Item::new(
            target,
            json!({"type": "send", "text": "go", "client_id": "tab"}),
        )])
        .await
        .remove(0)
    });
    until_requests(&fp, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await; // some text has streamed
    let out = as_agent(
        "rx",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "stop"}))]),
    )
    .await
    .remove(0);
    assert_eq!(out.result, json!({"stopped": true}), "{:?}", out.result);
    assert_eq!(turn.await.unwrap().status, Status::Cancelled);
    let seen = types(&drain_parts(&mut w));
    assert_eq!(
        seen.last().map(String::as_str),
        Some("part_abort"),
        "{seen:?}"
    );
    // the next turn sees what it was asked and the marker — never the half answer
    *fp.step.lock().unwrap() = Duration::ZERO;
    call(
        &kernel,
        ai.as_str(),
        json!({"type": "send", "text": "again"}),
    )
    .await;
    until_requests(&fp, 2).await;
    let nxt = fp.request(1);
    assert!(
        nxt.iter().any(|m| m["role"] == "user"
            && m["content"].as_str().is_some_and(|c| c.contains("go"))),
        "{nxt:?}"
    );
    let marker = last_assistant(&fp, 1);
    assert!(
        marker.starts_with("[stopped by rx]\npartial words"),
        "{}",
        marker.chars().take(80).collect::<String>()
    );
}

#[tokio::test]
async fn stop_with_nothing_in_flight_says_so() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_idle")).await;
    slow(&ai, &[], Duration::ZERO);
    let out = call(&kernel, ai.as_str(), json!({"type": "stop"})).await;
    assert_eq!(out, json!({"stopped": false}));
}

/// `rows` is the agent's in-memory head, as held — no store read, no transform:
/// nothing loaded → loaded:false; after a turn → its rows, paged by row id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_is_a_view_of_what_the_agent_holds() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_rows")).await;
    ram_store(&kernel, "viewer").await;
    wire(&kernel, "viewer", ai.as_str(), json!(["rows"]));
    let rows = |payload: Value| {
        let k = Arc::clone(&kernel);
        let target = ai.clone();
        async move {
            as_agent("viewer", k.send(vec![Item::new(target, payload)]))
                .await
                .remove(0)
                .result
        }
    };
    let cold = rows(json!({"type": "rows"})).await;
    assert_eq!(
        cold,
        json!({"rows": [], "next": null, "total": 0, "loaded": false}),
        "{cold}"
    );
    let fp = slow(&ai, &["hello there", DONE], Duration::ZERO);
    call(&kernel, ai.as_str(), json!({"type": "send", "text": "hi"})).await;
    until_requests(&fp, 2).await;
    // pass 2 (the nudge's answer) appends its rows AFTER its request starts: wait
    // until the head holds the whole turn — hi, hello there, the nudge, done — so
    // the two reads below page one settled head (each `rows` call is atomic; two
    // calls are not one snapshot)
    let mut settled = Value::Null;
    for _ in 0..250 {
        settled = rows(json!({"type": "rows"})).await;
        if settled["total"] == json!(4) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(settled["total"], json!(4), "{settled}");
    let page = rows(json!({"type": "rows", "limit": 1})).await;
    let rest = rows(json!({"type": "rows", "after": page["next"]})).await;
    assert_eq!(page["loaded"], json!(true), "{page}");
    let total = page["total"].as_u64().expect("a total");
    assert!(total >= 2, "{page}");
    assert_eq!(page["rows"][0]["role"], json!("user"), "{page}");
    assert!(
        page["rows"][0]["content"]
            .as_str()
            .is_some_and(|c| c.contains("hi")),
        "{page}"
    );
    let n = |v: &Value| v["rows"].as_array().map_or(0, Vec::len) as u64;
    assert_eq!(n(&page) + n(&rest), total, "{page} {rest}");
    assert_eq!(rest["next"], Value::Null, "{rest}");
    // an ungranted caller is refused by the gate
    ram_store(&kernel, "stranger").await;
    let out = as_agent(
        "stranger",
        kernel.send(vec![Item::new(ai.clone(), json!({"type": "rows"}))]),
    )
    .await
    .remove(0);
    assert_eq!(
        out.result["reason"],
        json!("unauthorized"),
        "{:?}",
        out.result
    );
}

// ─── the match: draft ↔ final message ────────────────────────────────────────

/// AI → AI: the consumer watches `part_*` on its inbox, then receives the
/// message; the message's header names the same stream id, `part_start` names
/// the message the producer answers (`re`, the draft's anchor), `part_end` says
/// ok.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_carries_the_stream_it_was_previewed_as() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let prod_id = make_ai(&kernel, &id_for(&tmp, "sp_prod")).await;
    let cons_id = make_ai(&kernel, &id_for(&tmp, "sp_cons")).await;
    wire(&kernel, prod_id.as_str(), cons_id.as_str(), json!("*"));
    let mut rx = watcher(&kernel, cons_id.as_str(), "w_cons").await;
    let msg = format!(
        "<call name=\"send\">\n<target_id>{}</target_id>\n<type>send</type>\n\
         <text>hello consumer</text>\n</call>",
        cons_id.as_str()
    );
    let prod = slow(&prod_id, &[&msg, DONE], Duration::ZERO);
    let cons = slow(&cons_id, &[DONE], Duration::ZERO);
    call(
        &kernel,
        prod_id.as_str(),
        json!({"type": "send", "text": "go"}),
    )
    .await;
    until_requests(&cons, 1).await;
    let parts = drain_parts(&mut rx);
    let start = parts
        .iter()
        .find(|e| e["type"] == "part_start")
        .expect("a part_start")
        .clone();
    let end = parts
        .iter()
        .find(|e| e["type"] == "part_end")
        .expect("a part_end")
        .clone();
    assert_eq!(start["target"], json!(cons_id.as_str()), "{start}");
    assert!(start.get("re").is_some_and(Value::is_i64), "{start}");
    assert_eq!(end["ok"], json!(true), "{end}");
    assert_eq!(end["stream_id"], start["stream_id"], "{end}");
    let delivered = cons
        .request(0)
        .last()
        .and_then(|m| m["content"].as_str())
        .expect("the delivered message")
        .to_string();
    let sid = start["stream_id"].as_str().unwrap();
    assert!(
        delivered
            .lines()
            .next()
            .unwrap_or_default()
            .contains(&format!("stream {sid}")),
        "{delivered}"
    );
    assert!(delivered.contains("hello consumer"), "{delivered}");
    assert!(prod.requests() >= 1);
}

#[test]
fn a_malformed_closed_call_ends_not_ok() {
    let (ev, _) = run(
        "<call name=\"send\"><target_id>x</target_id><type>a<path>p</path></call>",
        3,
    );
    let ok = ev
        .iter()
        .find_map(|e| match e {
            PartEvent::End(p, ok) if p.kind == PartKind::Call => Some(*ok),
            _ => None,
        })
        .expect("the call part ended");
    assert_eq!(ok, Some(false));
}

/// The in-flight entry holds the live splitter: a `status` snapshot never
/// carries it (it is bookkeeping, and not JSON).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_during_a_stream_is_json() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_st")).await;
    let words = "words ".repeat(300);
    let fp = slow(&ai, &[&words, DONE], Duration::from_millis(10));
    let k = Arc::clone(&kernel);
    let target = ai.clone();
    let turn = tokio::spawn(async move {
        k.send(vec![Item::new(
            target,
            json!({"type": "send", "text": "go"}),
        )])
        .await
        .remove(0)
    });
    until_requests(&fp, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let out = call(&kernel, ai.as_str(), json!({"type": "status"})).await;
    serde_json::to_string(&out).expect("a status is JSON");
    assert!(
        out.get("current")
            .and_then(Value::as_object)
            .is_none_or(|c| !c.contains_key("parts")),
        "{out}"
    );
    turn.await.unwrap();
}

/// Only a successful generation dispatches: a pass that wrote
/// a complete call and then stopped with another block open sends NEITHER, and the
/// marker names both as not sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_pass_sends_nothing_not_even_its_closed_calls() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_all")).await;
    ram_store(&kernel, "notes").await;
    wire(&kernel, ai.as_str(), "notes", json!("*"));
    let done_one = "<call name=\"send\"><target_id>notes</target_id><type>reflect</type></call>";
    let open_two = "<call name=\"send\"><target_id>notes</target_id><type>refl";
    let pass = format!("{done_one} then {open_two}");
    let fp = slow(&ai, &[&pass, DONE], Duration::ZERO);
    call(&kernel, ai.as_str(), json!({"type": "send", "text": "go"})).await;
    until_requests(&fp, 2).await;
    let marker = last_assistant(&fp, 1);
    assert_eq!(
        marker.lines().collect::<Vec<_>>(),
        vec![
            "[model stopped generating]",
            "then",
            "[a send call to notes was not sent]",
            "[a send call to notes was not sent]",
        ],
        "{marker}"
    );
    // nothing reached notes: no reply came back for the closed call either
    let nxt = fp.request(1);
    let last = nxt.last().unwrap()["content"].to_string();
    assert!(!last.contains("[reply to"), "{last}");
}

/// Deltas go to the addressee's inbox only: the state stream
/// carries a part's lifecycle — start / end / abort — never a delta.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deltas_never_reach_the_state_stream() {
    let tmp = TempDir::new().unwrap();
    let kernel = slow_kernel(&tmp).await;
    let ai = make_ai(&kernel, &id_for(&tmp, "sp_q")).await;
    ram_store(&kernel, "notes").await;
    wire(&kernel, ai.as_str(), "notes", json!("*"));
    let mut rx = watcher(&kernel, "notes", "w_q").await;
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let tap = Arc::clone(&seen);
    let _sub = kernel.add_state_subscriber(Arc::new(move |ev: &Value, _| {
        tap.lock().unwrap().push(ev.clone());
    }));
    let fp = slow(&ai, &[CALL, DONE], Duration::ZERO);
    call(&kernel, ai.as_str(), json!({"type": "send", "text": "go"})).await;
    until_requests(&fp, 2).await;
    let inbox = types(&drain_parts(&mut rx));
    assert!(inbox.iter().any(|t| t == "part_delta"), "{inbox:?}");
    assert_eq!(
        inbox.first().map(String::as_str),
        Some("part_start"),
        "{inbox:?}"
    );
    assert_eq!(
        inbox.last().map(String::as_str),
        Some("part_end"),
        "{inbox:?}"
    );
    let on_stream: std::collections::BTreeSet<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| e["payload"]["type"].as_str())
        .filter(|t| t.starts_with("part_"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        on_stream,
        ["part_end", "part_start"].map(String::from).into(),
        "{on_stream:?}"
    );
}
