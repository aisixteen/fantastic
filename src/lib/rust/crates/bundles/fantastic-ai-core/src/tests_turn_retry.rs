//! The pass path of a turn and of its helpers (#766, spec 2.7 and H11): retries
//! inside the turn, and the task pass the harness's helpers make. The cases of the old
//! stream checker (`tests_stream_cut.rs`, #499) live here with their fixtures: a foreign
//! tag is no longer cut and corrected by the router, it fails the attempt and is
//! re-asked inside the turn.

use super::*;
use crate::agent_loop::{run_generation, GenFailure, Generation};
use crate::model_format::default::Tags;
use crate::model_format::hermes::{Hermes, NAME as HERMES};
use crate::model_format::pass::Pass;
use crate::model_format::{Out, Reader, Renderer};
use futures_util::StreamExt;

/// A provider that answers every request with one canned text and logs what it was
/// sent.
struct Canned {
    answer: String,
    asked: std::sync::Mutex<Vec<Vec<Value>>>,
}

impl Canned {
    fn new(answer: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.to_string(),
            asked: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<Vec<Value>> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for Canned {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.asked.lock().unwrap().push(messages.to_vec());
        let ev: Vec<Result<ProviderEvent, ProviderError>> =
            vec![Ok(ProviderEvent::Token(self.answer.clone()))];
        Ok(Box::pin(futures_util::stream::iter(ev)))
    }
    fn model(&self) -> String {
        "qwen3-8b".to_string()
    }
}

// H11
/// A task pass (the fold summarizer) sends its messages rendered for the pass's type,
/// asks the provider once, and gives its answer back as text: a `<tool_call>` in it is
/// never parsed, only the reasoning is stripped.
#[tokio::test]
async fn task_pass_not_parsed() {
    let call = call_envelope("probe", json!({"type": "say", "text": "hi"}), None);
    let rows = vec![
        json!({"role": "user", "id": 1, "content": "go"}),
        json!({"role": "assistant", "id": 2, "content": format!("on it\n{call}")}),
    ];
    let held = "<tool_call>\n{\"name\":\"done\",\"arguments\":{}}\n</tool_call>";
    let answer = format!("<think>I will write {call}</think>TITLE\n\n{held}");

    // what the helper sends with no type of its own (the internal form)
    let plain = Canned::new(&answer);
    let (plain_text, _) =
        crate::strategies::summarize(&Pass::for_test(plain.clone()), &rows, None).await;
    let internal = plain.asked();
    assert_eq!(internal.len(), 1);

    let provider = Canned::new(&answer);
    let state = crate::state::state_for(&AgentId::from("h11_task_pass"));
    let pass = Pass::typed_for_test(
        provider.clone(),
        Arc::clone(&state),
        HERMES,
        Some("qwen3-8b"),
    );
    let before = state.render_cache.stats();
    let (text, cut) = crate::strategies::summarize(&pass, &rows, None).await;

    let asked = provider.asked();
    assert_eq!(asked.len(), 1, "the provider is called once");
    assert_eq!(asked[0].len(), internal[0].len());
    for (sent, was) in asked[0].iter().zip(&internal[0]) {
        assert_eq!(sent["role"], was["role"]);
        let was = was["content"].as_str().expect("text content");
        assert_eq!(
            sent["content"],
            json!(Hermes.render(was)),
            "each message mapped for the type: one stored text through its renderer"
        );
    }
    let sent = Value::Array(asked[0].clone()).to_string();
    assert!(sent.contains("<tool_call>"), "{sent}");
    assert!(
        !sent.contains("<call name="),
        "no internal call form left: {sent}"
    );

    // the answer is plain text: the call form in it reaches the caller as written
    assert_eq!(text, format!("TITLE\n\n{held}"));
    assert_eq!(text, plain_text, "the type does not change the answer");
    assert!(!cut);
    assert_eq!(
        state.render_cache.stats(),
        before,
        "a task pass does not use the cache"
    );
    crate::state::drop_state(&AgentId::from("h11_task_pass"));
}

// ─── the attempts of a turn (#766, 1.5 step 4; spec 2.7) ─────────────────────

const DONE: &str = r#"<call name="done"></call>"#;
/// A foreign call tag: the `default` reader fails the attempt at it (#499's fixture).
const FOREIGN: &str = "Sending.\n<invoke name=\"send\">";
/// The model id of a `hermes` chat (`res/model_types.json`).
const QWEN: &str = "qwen3-8b";
/// A model id of the `default` type.
const PLAIN: &str = "test-model";

/// The assistant rows the agent's store holds, whole.
async fn assistant_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    store_rows(kernel, agent)
        .await
        .into_iter()
        .filter(|r| r["role"] == json!("assistant"))
        .collect()
}

/// The `model_switched` trace rows of the agent's store.
async fn switches(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    store_rows(kernel, agent)
        .await
        .into_iter()
        .filter(|r| r["kind"] == json!("trace") && r["event"] == json!("model_switched"))
        .collect()
}

/// Every row the agent's store holds.
async fn store_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

/// The assistant rows' text the agent's store holds.
async fn answers(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<String> {
    assistant_rows(kernel, agent)
        .await
        .iter()
        .filter_map(|r| r["content"].as_str().map(str::to_string))
        .collect()
}

async fn send_go(kernel: &Arc<Kernel>, backend: &AgentId) -> Value {
    kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await
        .remove(0)
        .result
}

fn chars(text: &str) -> Vec<ScriptEvent> {
    text.chars()
        .map(|c| ScriptEvent::Token(c.to_string()))
        .collect()
}

fn reflect() -> String {
    call_envelope("kernel", json!({"type": "reflect", "tree": "none"}), None)
}

/// `internal` in the `hermes` type's call form: what a hermes model writes.
fn hermes(internal: &str) -> String {
    Hermes.render(internal)
}

/// The report the `default` reader gives for the whole answer `text`, if any.
fn default_report(text: &str) -> Option<String> {
    let mut out = Tags.outbound();
    let mut all = out.feed(text);
    all.extend(out.finish());
    all.into_iter().find_map(|o| match o {
        Out::Bad { report } => Some(report),
        _ => None,
    })
}

/// The report the `hermes` reader gives for the whole answer `text`, if any.
fn hermes_report(text: &str) -> Option<String> {
    let mut out = Hermes.outbound();
    let mut all = out.feed(text);
    all.extend(out.finish());
    all.into_iter().find_map(|o| match o {
        Out::Bad { report } => Some(report),
        _ => None,
    })
}

/// Set a chat's record `model`, so its passes run under that model's type.
async fn set_model(kernel: &Arc<Kernel>, agent: &AgentId, model: &str) {
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "model": model}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
}

/// Set a chat's `transport_retries`.
async fn set_transport_retries(kernel: &Arc<Kernel>, agent: &AgentId, n: i64) {
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": agent.as_str(), "transport_retries": n}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
}

/// A chat whose record and provider name a `hermes` model.
async fn mk_hermes(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let (kernel, backend) = mk_kernel(tmp, tag).await;
    set_model(&kernel, &backend, QWEN).await;
    (kernel, backend)
}

/// Script `agent` and name the model its endpoint says served each pass.
fn script_as(agent: &AgentId, model: &str, passes: Vec<Vec<ScriptEvent>>) -> Arc<MockScript> {
    let s = script(agent, passes, Duration::ZERO);
    *s.model.lock().unwrap() = model.to_string();
    s
}

/// A watcher on `agent`'s events: tokens, statuses and parts, in order.
async fn watch(kernel: &Arc<Kernel>, agent: &AgentId) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from(format!("w_{}", agent.as_str()).as_str());
    let rx = kernel.ensure_inbox(&w);
    assert!(fantastic_kernel::send::as_operator(kernel.watch(agent, w))
        .await
        .is_none());
    rx
}

/// Events on `rx` until one matches `end` (included), bounded by 5 s.
async fn events_until(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    end: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..200 {
        while let Some(v) = rx.try_recv() {
            let stop = end(&v.payload);
            out.push(v.payload);
            if stop {
                return out;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    out
}

fn is_status(v: &Value, phase: &str) -> bool {
    v["type"] == json!("status") && v["phase"] == json!(phase)
}

/// A status that ends a call.
fn ends_call(v: &Value) -> bool {
    v["type"] == json!("status")
        && ["done", "failed", "stalled", "awaiting"]
            .contains(&v["phase"].as_str().unwrap_or_default())
}

/// A provider for driving `run_generation` directly: it serves scripted attempts in
/// order, logs every request, names the model of the attempt it served last, and counts
/// the streams it served that were dropped before their end.
struct Steps {
    attempts: std::sync::Mutex<std::collections::VecDeque<Step>>,
    asked: std::sync::Mutex<Vec<Vec<Value>>>,
    model: std::sync::Mutex<String>,
    dropped: Arc<std::sync::atomic::AtomicUsize>,
}

/// One scripted attempt.
struct Step {
    tokens: Vec<String>,
    /// The model the endpoint names for it.
    model: &'static str,
    /// After its tokens the stream stays open: only a drop ends it.
    pending: bool,
    /// After its tokens the link fails.
    transport: bool,
}

impl Step {
    fn says(text: &str, model: &'static str) -> Self {
        Self {
            tokens: vec![text.to_string()],
            model,
            pending: false,
            transport: false,
        }
    }
}

/// Counts a drop.
struct DropFlag(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Steps {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            attempts: std::sync::Mutex::new(steps.into_iter().collect()),
            asked: std::sync::Mutex::new(Vec::new()),
            model: std::sync::Mutex::new(String::new()),
            dropped: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    fn asked(&self) -> Vec<Vec<Value>> {
        self.asked.lock().unwrap().clone()
    }

    fn dropped(&self) -> usize {
        self.dropped.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for Steps {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.asked.lock().unwrap().push(messages.to_vec());
        let step = self
            .attempts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Step::says(DONE, ""));
        if !step.model.is_empty() {
            *self.model.lock().unwrap() = step.model.to_string();
        }
        let mut evs: Vec<Result<ProviderEvent, ProviderError>> = step
            .tokens
            .into_iter()
            .map(|t| Ok(ProviderEvent::Token(t)))
            .collect();
        if step.transport {
            evs.push(Err(ProviderError::transport("link dropped")));
        }
        let head = futures_util::stream::iter(evs);
        if !step.pending {
            return Ok(Box::pin(head));
        }
        let flag = DropFlag(Arc::clone(&self.dropped));
        let tail = futures_util::stream::pending().map(move |e| {
            let _ = &flag;
            e
        });
        Ok(Box::pin(head.chain(tail)))
    }
    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }
}

/// One generation of `agent` on `provider`, run as the agent, as `send` runs it.
async fn generate(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
    provider: &Arc<Steps>,
) -> Result<Generation, GenFailure> {
    let p: Arc<dyn Provider> = Arc::clone(provider) as Arc<dyn Provider>;
    let state = crate::state::state_for(agent);
    fantastic_kernel::test_support::as_agent(
        agent.clone(),
        run_generation(&p, agent, &state, "go", kernel, "probe", CFG),
    )
    .await
}

/// The generation's answer text and whether it was cut; panics on anything else.
fn answered(g: Result<Generation, GenFailure>) -> (String, bool) {
    match g {
        Ok(Generation::Answered { text, cut, .. }) => (text, cut),
        Ok(Generation::Stopped { reason, .. }) => panic!("stopped: {reason}"),
        Err(_) => panic!("the generation failed"),
    }
}

// T1
/// A bad call, then a good one: one assistant row, from attempt 2; no stored row holds
/// attempt 1's text or its report; the router gets attempt 2's text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_leaves_no_trace() {
    let wrapped = "<tool_call>\n<call name=\"done\"></call>\n</tool_call>";
    let report = default_report(wrapped).expect("the default reader fails it");
    for kind in ["default", HERMES] {
        let tmp = TempDir::new().unwrap();
        let (kernel, backend) = mk_kernel(&tmp, &format!("tr_t1_{kind}")).await;
        let (bad, good, done) = if kind == HERMES {
            set_model(&kernel, &backend, QWEN).await;
            // the default form is foreign to a hermes model (S9)
            (DONE.to_string(), hermes(&reflect()), hermes(DONE))
        } else {
            (wrapped.to_string(), reflect(), DONE.to_string())
        };
        let model = if kind == HERMES { QWEN } else { PLAIN };
        let s = script_as(&backend, model, vec![pass(&bad), pass(&good), pass(&done)]);
        send_go(&kernel, &backend).await;
        assert!(
            until_passes(&s, 3).await,
            "{kind}: the reply never came back"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let rows = answers(&kernel, &backend).await;
        assert_eq!(rows[0], reflect(), "{kind}: {rows:?}");
        for row in store_rows(&kernel, &backend).await {
            let text = row["content"].as_str().unwrap_or_default();
            assert!(!text.contains("<tool_call>\n<call"), "{kind}: {row}");
            assert!(!text.contains(&report), "{kind}: {row}");
        }
        // the router got attempt 2's call: its reply is the next turn
        let next = s.request(2);
        assert!(next.contains("kernel reflect"), "{kind}: {next}");
    }
}

/// A malformed attempt of `kind` that the reader still refuses: for `default` a call
/// wrapped in `<tool_call>`, for `hermes` a body both in the JSON and in a block.
fn still_malformed(kind: &str) -> String {
    if kind == HERMES {
        format!(
            "<tool_call>\n{}\n<bytes>a</bytes>\n</tool_call>",
            r#"{"name": "send", "arguments": {"routing": {"target_id": "notes"}, "payload": {"type": "write", "bytes": "x"}}}"#
        )
    } else {
        "<tool_call>\n<call name=\"done\"></call>\n</tool_call>".to_string()
    }
}

/// The report of `kind`'s reader for the whole answer `text`.
fn report_of(kind: &str, text: &str) -> String {
    if kind == HERMES {
        hermes_report(text)
    } else {
        default_report(text)
    }
    .expect("the reader fails it")
}

/// A `done` status whose reason is `cancelled`.
fn is_cancelled(v: &Value) -> bool {
    is_status(v, "done") && v["detail"]["reason"] == json!("cancelled")
}

/// A malformed attempt, then a good one (the first GPU run, #766): the events the e2e
/// harness reads are `retrying` with the error, then `streaming`, then `done` with
/// reason `ok`; no `failed`, no cancel. Exactly two requests, the second carrying the
/// re-ask pair; the stored answer is attempt 2's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_then_reask_completes() {
    for kind in ["default", HERMES] {
        let tmp = TempDir::new().unwrap();
        let (kernel, backend) = mk_kernel(&tmp, &format!("tr_reask_{kind}")).await;
        let bad = still_malformed(kind);
        // the internal `done` as the reader gives it back, for either type
        let done_call = crate::tool_parse::render_call("done", &Map::new());
        let good = if kind == HERMES {
            set_model(&kernel, &backend, QWEN).await;
            hermes(&done_call)
        } else {
            done_call.clone()
        };
        let report = report_of(kind, &bad);
        let model = if kind == HERMES { QWEN } else { PLAIN };
        let s = script_as(&backend, model, vec![pass(&bad), pass(&good)]);
        let mut rx = watch(&kernel, &backend).await;
        send_go(&kernel, &backend).await;
        let evs = events_until(&mut rx, |v| is_status(v, "done")).await;
        let retry = evs
            .iter()
            .position(|v| is_status(v, "retrying"))
            .unwrap_or_else(|| panic!("{kind}: no retrying status: {evs:#?}"));
        let d = &evs[retry]["detail"];
        assert_eq!(d["attempt"], json!(1), "{kind}: {d}");
        assert_eq!(d["kind"], json!("call"), "{kind}: {d}");
        assert_eq!(
            d["error"],
            json!(report.lines().next().unwrap_or_default()),
            "{kind}: {d}"
        );
        let streaming = evs[retry..]
            .iter()
            .position(|v| is_status(v, "streaming"))
            .map(|i| retry + i)
            .unwrap_or_else(|| panic!("{kind}: no streaming after the retry: {evs:#?}"));
        let done = evs
            .iter()
            .position(|v| is_status(v, "done"))
            .unwrap_or_else(|| panic!("{kind}: the call never ended: {evs:#?}"));
        assert!(retry < streaming && streaming < done, "{kind}: {evs:#?}");
        assert_eq!(
            evs[done]["detail"]["reason"],
            json!("ok"),
            "{kind}: {:?}",
            evs[done]
        );
        assert!(
            !evs.iter()
                .any(|v| is_status(v, "failed") || is_cancelled(v)),
            "{kind}: {evs:#?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(s.passes_served(), 2, "{kind}");
        let view = s.request_messages(0);
        let n = view.len();
        let a2 = s.request_messages(1);
        assert_eq!(a2[..n], view[..], "{kind}: attempt 2 starts with the view");
        assert_eq!(
            a2[n..],
            [
                json!({"role": "assistant", "content": bad}),
                json!({"role": "user", "content": report}),
            ][..],
            "{kind}"
        );
        assert_eq!(answers(&kernel, &backend).await, vec![done_call], "{kind}");
    }
}

/// Every attempt malformed: `CALL_RETRIES` `retrying` statuses, each with its error,
/// then the harness stop ends the call `failed` with reason `gave_up` and the stop's
/// reason as its error; no cancel. Exactly `1 + CALL_RETRIES` requests; the stored
/// answer is the `stop` call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_malformed_gives_up() {
    let n = 1 + crate::constants::CALL_RETRIES;
    for kind in ["default", HERMES] {
        let tmp = TempDir::new().unwrap();
        let (kernel, backend) = mk_kernel(&tmp, &format!("tr_giveup_{kind}")).await;
        let bad = still_malformed(kind);
        let wording = if kind == HERMES {
            set_model(&kernel, &backend, QWEN).await;
            crate::model_format::hermes::STOP_REASON
        } else {
            crate::model_format::default::STOP_REASON
        };
        let reason = crate::model_format::fill(wording, &[("n", &n.to_string())]);
        let model = if kind == HERMES { QWEN } else { PLAIN };
        let s = script_as(&backend, model, (0..8).map(|_| pass(&bad)).collect());
        let mut rx = watch(&kernel, &backend).await;
        send_go(&kernel, &backend).await;
        let evs = events_until(&mut rx, |v| is_status(v, "failed")).await;
        let retries: Vec<&Value> = evs.iter().filter(|v| is_status(v, "retrying")).collect();
        assert_eq!(
            retries.len(),
            crate::constants::CALL_RETRIES,
            "{kind}: {evs:#?}"
        );
        for (i, r) in retries.iter().enumerate() {
            assert_eq!(r["detail"]["attempt"], json!(i + 1), "{kind}: {r}");
            assert_eq!(r["detail"]["kind"], json!("call"), "{kind}: {r}");
            assert!(
                r["detail"]["error"].as_str().is_some_and(|e| !e.is_empty()),
                "{kind}: {r}"
            );
        }
        let st = evs.last().expect("an event");
        assert!(is_status(st, "failed"), "{kind}: {evs:#?}");
        assert_eq!(st["detail"]["reason"], json!("gave_up"), "{kind}: {st}");
        assert_eq!(st["detail"]["error"], json!(reason), "{kind}: {st}");
        assert!(!evs.iter().any(is_cancelled), "{kind}: {evs:#?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(s.passes_served(), n, "{kind}");
        let mut args = Map::new();
        args.insert("reason".to_string(), json!(reason));
        assert_eq!(
            answers(&kernel, &backend).await,
            vec![crate::tool_parse::render_call("stop", &args)],
            "{kind}"
        );
    }
}

// T2
/// The re-ask request is the typed view, then per failed attempt its raw text without
/// thinking as the assistant and its report, in the type's wording, as the user;
/// attempt 3 carries both pairs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reask_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t2").await;
    let first = format!("<think>I will use my native tool.</think>{FOREIGN}");
    let second = "<tool_call>\n<call name=\"done\"></call>\n</tool_call>";
    let third = "<think>now right</think>Done.\n<call name=\"done\"></call>";
    let s = script_as(
        &backend,
        PLAIN,
        vec![pass(&first), pass(second), pass(third)],
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 3).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let view = s.request_messages(0);
    let n = view.len();
    let r1 = default_report(&first).expect("attempt 1 is bad");
    let r2 = default_report(second).expect("attempt 2 is bad");
    let pair1 = [
        json!({"role": "assistant", "content": FOREIGN}),
        json!({"role": "user", "content": r1}),
    ];
    let pair2 = [
        json!({"role": "assistant", "content": second}),
        json!({"role": "user", "content": r2}),
    ];
    let a2 = s.request_messages(1);
    assert_eq!(a2[..n], view[..], "attempt 2 starts with the view");
    assert_eq!(a2[n..], pair1[..]);
    let a3 = s.request_messages(2);
    assert_eq!(a3[..n], view[..]);
    assert_eq!(a3[n..n + 2], pair1[..]);
    assert_eq!(a3[n + 2..], pair2[..]);
    let all = Value::Array(a3).to_string();
    assert!(!all.contains("native tool"), "{all}");
    assert!(!all.contains("<think>"), "{all}");
    let rows = assistant_rows(&kernel, &backend).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["content"],
        json!("Done.\n<call name=\"done\"></call>")
    );
    assert_eq!(rows[0]["thinking"], json!("now right"));
}

// T13
/// Reasoning in a failed attempt is in no re-ask's assistant message; the stored row's
/// `thinking` is the accepted attempt's (#499's fixture: thinking, then a foreign tag).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_thinking() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t13").await;
    let mut first =
        chars("<think>I will use my native tool.</think>Sending.\n<invoke name=\"send\">");
    first.extend((0..2000).map(|_| ScriptEvent::Token("\n</invoke".into())));
    let s = script_as(
        &backend,
        PLAIN,
        vec![
            first,
            pass("<think>the done call</think><call name=\"done\"></call>"),
        ],
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 2).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reask = s.request_messages(1);
    let n = reask.len();
    assert_eq!(reask[n - 2]["role"], json!("assistant"));
    let raw = reask[n - 2]["content"].as_str().unwrap_or_default();
    assert!(raw.contains("Sending."), "{raw}");
    assert!(!raw.contains("native tool"), "{raw}");
    assert!(!raw.contains("<think>"), "{raw}");
    let rows = assistant_rows(&kernel, &backend).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["content"], json!(DONE));
    assert_eq!(rows[0]["thinking"], json!("the done call"));
}

// T3
/// Attempt 1 writes a good call then a bad one; attempt 2 the good call and a fixed one:
/// the good call runs once, attempt 2's; nothing of attempt 1 is dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_attempt_dispatches_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_t3").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let fb = script(&b, vec![pass(DONE), pass(DONE)], Duration::ZERO);
    let call_b = call_envelope(b.as_str(), json!({"type": "send", "text": "hi"}), None);
    let s = script_as(
        &a,
        PLAIN,
        vec![
            pass(format!(
                "{call_b}\n<send><target_id>kernel</target_id><type>ps</type></send>"
            )),
            pass(format!("{call_b}\n{}", reflect())),
            pass(DONE),
        ],
    );
    send_go(&kernel, &a).await;
    assert!(until_passes(&s, 3).await, "the replies never came back");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fb.passes_served(), 1, "the peer ran once: attempt 2's call");
    let batch = last_content(&s, 2);
    assert_eq!(batch.matches("kernel reflect").count(), 1, "{batch}");
}

// T4, T9 (status half in tests_stopped.rs)
/// Every attempt bad: the provider is called exactly `1 + CALL_RETRIES` times and never
/// again in that send; the turn ends `Stopped`, its reply the stored `stop` call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_after_n() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t4").await;
    let s = script_as(&backend, PLAIN, (0..8).map(|_| pass(FOREIGN)).collect());
    let reply = send_go(&kernel, &backend).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = 1 + crate::constants::CALL_RETRIES;
    assert_eq!(s.passes_served(), n);
    assert!(reply["message_id"].as_i64().is_some(), "{reply}");
    let response = reply["response"].as_str().unwrap_or_default();
    assert!(response.starts_with("<call name=\"stop\">"), "{reply}");
    let reason = crate::model_format::fill(
        crate::model_format::default::STOP_REASON,
        &[("n", &n.to_string())],
    );
    assert!(response.contains(&reason), "{reply}");

    // the same through `run_generation`: `Generation::Stopped`
    let tmp2 = TempDir::new().unwrap();
    let (kernel2, backend2) = mk_kernel(&tmp2, "tr_t4b").await;
    let steps = Steps::new((0..8).map(|_| Step::says(FOREIGN, PLAIN)).collect());
    match generate(&kernel2, &backend2, &steps).await {
        Ok(Generation::Stopped { reason: r, .. }) => assert_eq!(r, reason),
        _ => panic!("not stopped"),
    }
    assert_eq!(steps.asked().len(), n);
}

// T5
/// A failed attempt: status `retrying {attempt, of, error, kind: "call"}` first, then its
/// open parts abort with the reason `malformed`. A transport rerun's status has no `kind`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_marks_then_aborts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t5").await;
    let _s = script_as(&backend, PLAIN, vec![chars(FOREIGN), pass(DONE)]);
    let mut rx = watch(&kernel, &backend).await;
    send_go(&kernel, &backend).await;
    let evs = events_until(&mut rx, ends_call).await;
    let retry = evs
        .iter()
        .position(|v| is_status(v, "retrying"))
        .expect("a retrying status");
    let abort = evs
        .iter()
        .position(|v| v["type"] == json!("part_abort") && v["reason"] == json!("malformed"))
        .expect("the open text part aborts");
    assert!(retry < abort, "{evs:#?}");
    let detail = &evs[retry]["detail"];
    let report = default_report(FOREIGN).expect("bad");
    assert_eq!(detail["attempt"], json!(1));
    assert_eq!(detail["of"], json!(crate::constants::CALL_RETRIES));
    assert_eq!(detail["kind"], json!("call"));
    assert_eq!(
        detail["error"],
        json!(report.lines().next().unwrap_or_default())
    );

    // a transport rerun keeps today's three keys, no `kind`
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t5b").await;
    set_transport_retries(&kernel, &backend, 1).await;
    let _s = script_as(
        &backend,
        PLAIN,
        vec![
            vec![ScriptEvent::TransportFail("connect failed".into())],
            pass(DONE),
        ],
    );
    let mut rx = watch(&kernel, &backend).await;
    send_go(&kernel, &backend).await;
    let evs = events_until(&mut rx, ends_call).await;
    let retry = evs
        .iter()
        .find(|v| is_status(v, "retrying"))
        .expect("a retrying status");
    assert!(retry["detail"].get("kind").is_none(), "{retry}");
    assert_eq!(retry["detail"]["of"], json!(1));
}

// T6
/// A transport error mid-attempt and mid-re-ask: today's path, the turn is rerun whole
/// from attempt 1; no stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_error_unchanged() {
    // mid-attempt
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t6a").await;
    set_transport_retries(&kernel, &backend, 1).await;
    let s = script_as(
        &backend,
        PLAIN,
        vec![
            vec![
                ScriptEvent::Token("partial".into()),
                ScriptEvent::TransportFail("connect failed".into()),
            ],
            pass(DONE),
        ],
    );
    let reply = send_go(&kernel, &backend).await;
    assert_eq!(s.passes_served(), 2);
    assert!(s.request(1).contains("[retry 1/1]"), "{}", s.request(1));
    assert_eq!(reply["response"], json!(DONE), "{reply}");

    // mid-re-ask: the rerun starts at attempt 1, with no re-ask pair of the earlier run
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t6b").await;
    set_transport_retries(&kernel, &backend, 1).await;
    let s = script_as(
        &backend,
        PLAIN,
        vec![
            pass(FOREIGN),
            vec![ScriptEvent::TransportFail("connect failed".into())],
            pass(DONE),
        ],
    );
    let reply = send_go(&kernel, &backend).await;
    assert_eq!(s.passes_served(), 3);
    let rerun = s.request(2);
    assert!(rerun.contains("[retry 1/1]"), "{rerun}");
    assert!(!rerun.contains("<invoke"), "{rerun}");
    let report = default_report(FOREIGN).expect("bad");
    assert!(
        !s.request_messages(2)
            .iter()
            .any(|m| m["content"] == json!(report)),
        "{rerun}"
    );
    assert_eq!(reply["response"], json!(DONE), "{reply}");
}

// T7
/// A re-ask answered with nothing, reasoning only or prose only is no call either
/// (#795): it is re-asked again in the same turn, its report the no-call text, and the
/// third request still carries the first attempt's re-ask pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callless_reask_is_accepted() {
    for (i, answer) in ["", "<think>only a thought</think>", "just prose"]
        .into_iter()
        .enumerate()
    {
        let tmp = TempDir::new().unwrap();
        let (kernel, backend) = mk_kernel(&tmp, &format!("tr_t7_{i}")).await;
        let s = script_as(
            &backend,
            PLAIN,
            vec![pass(FOREIGN), pass(answer), pass(DONE)],
        );
        send_go(&kernel, &backend).await;
        assert!(until_passes(&s, 3).await, "{answer:?}: no third attempt");
        let last = last_in(&s, 2);
        assert_eq!(last, crate::model_format::default::REPORT_NO_CALL, "{answer:?}");
        // the third request is the same turn: the first re-ask pair is in it
        let report = default_report(FOREIGN).expect("bad");
        assert!(
            s.request_messages(2)
                .iter()
                .any(|m| m["content"] == json!(report)),
            "{answer:?}"
        );
    }
}

// T8, K41 (cancel)
/// The chat's `stop` or a cancel during a re-ask: the generation is dropped, no
/// assistant row comes from it, no pin is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_reask() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_t8").await;
    let s = script_as(
        &backend,
        QWEN,
        vec![
            pass(DONE),
            vec![ScriptEvent::Stall {
                idle: Duration::from_secs(30),
                endpoint: "slow".into(),
            }],
        ],
    );
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let send = tokio::spawn(async move { send_go(&k, &b).await });
    assert!(until_passes(&s, 2).await, "the re-ask never started");
    let table = kernel
        .operator_send(vec![Item::new(
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
        .expect("the send in `ps`");
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": task_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["cancelled"], json!(true), "{r}");
    let reply = send.await.expect("send join");
    assert_eq!(reply, json!({"cancelled": true}));
    let report = hermes_report(DONE).expect("bad for hermes");
    for row in store_rows(&kernel, &backend).await {
        let text = row["content"].as_str().unwrap_or_default();
        assert!(!text.contains(DONE), "{row}");
        assert!(!text.contains(&report), "{row}");
    }
    assert!(
        !store_rows(&kernel, &backend)
            .await
            .iter()
            .any(|r| r["kind"] == json!("trace")),
        "a cancelled turn writes no trace row"
    );
    let stats = crate::state::state_for(&backend).render_cache.stats();
    assert_eq!(stats.pins, 0);
    assert_eq!(s.passes_served(), 2, "no request after the cancel");
}

// T12
/// All attempts run inside one send ceiling: a slow second attempt ends the send with
/// today's timeout reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_share_the_ceiling() {
    let _guard = SEND_TIMEOUT_ENV_LOCK.lock().await;
    let _ceiling = Ceiling::set("0.4");
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t12").await;
    let s = script_as(
        &backend,
        PLAIN,
        vec![
            pass(FOREIGN),
            vec![ScriptEvent::Stall {
                idle: Duration::from_secs(5),
                endpoint: "slow".into(),
            }],
        ],
    );
    let started = std::time::Instant::now();
    let reply = send_go(&kernel, &backend).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(s.passes_served(), 2);
    assert_eq!(
        reply,
        json!({
            "error": "send: timeout after 0.4s",
            "reason": "timeout",
            "client_id": "probe"
        })
    );
}

// T14
/// A hermes call held while its body arrives slowly: no call text reaches the caller
/// during the hold, and the whole internal call arrives once at the close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_call_emits_once() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_t14").await;
    let internal = call_envelope(
        "kernel",
        json!({"type": "reflect", "bytes": "a long body\nline two"}),
        None,
    );
    let _s = script_as(
        &backend,
        QWEN,
        vec![chars(&hermes(&internal)), pass(hermes(DONE))],
    );
    let mut rx = watch(&kernel, &backend).await;
    send_go(&kernel, &backend).await;
    let evs = events_until(&mut rx, ends_call).await;
    let tokens: Vec<&str> = evs
        .iter()
        .filter(|v| v["type"] == json!("token"))
        .filter_map(|v| v["text"].as_str())
        .collect();
    let calls: Vec<&&str> = tokens.iter().filter(|t| t.contains("<call")).collect();
    assert_eq!(calls.len(), 1, "{tokens:?}");
    assert_eq!(*calls[0], internal.as_str());
    assert!(
        tokens.iter().all(|t| !t.contains("tool_call")),
        "{tokens:?}"
    );
}

// T15, S7, S18
/// A call cut by the token limit is no malformed call: the provider is called once, the
/// stored row is the cut marker naming every call of the pass as not sent, nothing is
/// dispatched (default and hermes; hermes with a complete call before the cut one).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_call_is_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_s7").await;
    let open = "<call name=\"send\"><routing><target_id>kernel</target_id></routing><payload>";
    let steps = Steps::new(vec![Step::says(open, PLAIN)]);
    let (_, cut) = answered(generate(&kernel, &backend, &steps).await);
    assert!(cut);
    assert_eq!(steps.asked().len(), 1, "no retry for a cut");
    let rows = answers(&kernel, &backend).await;
    assert!(
        rows[0].starts_with("[model stopped generating]"),
        "{rows:?}"
    );
    assert!(
        rows[0].contains("[a send call to kernel was not sent]"),
        "{rows:?}"
    );
    assert!(!rows[0].contains("<call"), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hermes_cut_no_retry() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_t15").await;
    let whole = hermes(&reflect());
    let cut_at = whole.find("\"payload\"").expect("a payload");
    let steps = Steps::new(vec![Step::says(&whole[..cut_at], QWEN)]);
    let (_, cut) = answered(generate(&kernel, &backend, &steps).await);
    assert!(cut);
    assert_eq!(steps.asked().len(), 1, "no retry for a cut");
    let rows = answers(&kernel, &backend).await;
    assert!(
        rows[0].starts_with("[model stopped generating]"),
        "{rows:?}"
    );
    assert!(
        rows[0].contains("[a send call to kernel was not sent]"),
        "{rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cut_lists_every_call() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_s18").await;
    let first = hermes(&call_envelope("notes", json!({"type": "read"}), None));
    let second = hermes(&reflect());
    let cut_at = second.find("\"payload\"").expect("a payload");
    let text = format!("{first}\n{}", &second[..cut_at]);
    let steps = Steps::new(vec![Step::says(&text, QWEN)]);
    let (_, cut) = answered(generate(&kernel, &backend, &steps).await);
    assert!(cut, "the pass is cut");
    assert_eq!(steps.asked().len(), 1);
    let rows = answers(&kernel, &backend).await;
    assert!(
        rows[0].contains("[a send call to notes was not sent]"),
        "{rows:?}"
    );
    assert!(
        rows[0].contains("[a send call to kernel was not sent]"),
        "{rows:?}"
    );
}

// T16
/// No cut streak is left: nine turns in a row that each needed a retry and then
/// answered go on; no give-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_cut_streak() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_t16").await;
    let mut passes = Vec::new();
    for _ in 0..9 {
        passes.push(pass(FOREIGN));
        passes.push(pass(reflect()));
    }
    passes.push(pass(DONE));
    let total = passes.len();
    let s = script_as(&a, PLAIN, passes);
    let mut rx = rebind_backend_inbox(&kernel, &a);
    send_go(&kernel, &a).await;
    assert!(until_passes(&s, total).await, "the chain stalled");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(s.passes_served(), total);
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        assert!(
            !(is_status(&p, "failed") && p["detail"]["reason"] == json!("gave_up")),
            "{p}"
        );
    }
}

// T19
/// `default` streams live, so a `Bad` known only at the stream's end follows tokens the
/// caller already has: they are not taken back. The order is attempt 1's tokens, the
/// `retrying` marker, the parts' aborts, attempt 2's tokens. A `send_to` target
/// receives nothing of the discarded attempt: it is handed the accepted text only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_then_bad_is_marked() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_t19").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let fb = script(&b, vec![pass(DONE)], Duration::ZERO);
    // a terminated envelope that does not parse: known only at the stream's end
    let bad = "Working.\n<call name=\"send\"><routing><reply_to>self</reply_to></routing>\
               <payload><type>reflect</type></payload></call>\nmore";
    assert!(default_report(bad).is_some());
    let _s = script_as(&a, PLAIN, vec![chars(bad), chars(DONE)]);
    let mut rx = watch(&kernel, &a).await;
    let _ = kernel
        .operator_send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "tab1", "send_to": b.as_str()}),
        )])
        .await;
    let evs = events_until(&mut rx, ends_call).await;
    let retry = evs
        .iter()
        .position(|v| is_status(v, "retrying"))
        .expect("a retrying status");
    assert_eq!(evs[retry]["detail"]["kind"], json!("call"));
    assert_eq!(evs[retry]["detail"]["attempt"], json!(1));
    let before: String = evs[..retry]
        .iter()
        .filter(|v| v["type"] == json!("token"))
        .filter_map(|v| v["text"].as_str())
        .collect();
    assert_eq!(before, bad, "attempt 1's tokens, none taken back");
    let abort = evs
        .iter()
        .position(|v| v["type"] == json!("part_abort") && v["reason"] == json!("malformed"))
        .expect("an abort");
    assert!(retry < abort, "{evs:#?}");
    let first_after = retry
        + evs[retry..]
            .iter()
            .position(|v| v["type"] == json!("token"))
            .expect("attempt 2's tokens");
    assert!(abort < first_after, "{evs:#?}");
    let after: String = evs[retry..]
        .iter()
        .filter(|v| v["type"] == json!("token"))
        .filter_map(|v| v["text"].as_str())
        .collect();
    assert_eq!(after, DONE);
    // the `send_to` hand-off carries the accepted text, nothing of attempt 1
    assert!(until_passes(&fb, 1).await, "the hand-off never came");
    let handed = last_in(&fb, 0);
    assert!(handed.contains(DONE), "{handed}");
    assert!(!handed.contains("Working."), "{handed}");
}

// T20
/// A transport error after one failed attempt: the rerun starts at attempt 1, with no
/// pin held and no re-ask pair of the earlier run in its request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rerun_restarts_attempts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_t20").await;
    set_transport_retries(&kernel, &backend, 1).await;
    // a stored history, so the hermes cache holds entries
    let s = script_as(&backend, QWEN, vec![pass(hermes(DONE))]);
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 1).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = script_as(
        &backend,
        QWEN,
        vec![
            pass(DONE),
            vec![ScriptEvent::TransportFail("connect failed".into())],
            pass(hermes(DONE)),
        ],
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 3).await);
    let report = hermes_report(DONE).expect("bad for hermes");
    let rerun = s.request_messages(2);
    assert!(
        !rerun.iter().any(|m| m["content"] == json!(report)),
        "{rerun:?}"
    );
    assert!(
        !rerun.iter().any(|m| m["content"] == json!(DONE)),
        "{rerun:?}"
    );
    assert_eq!(
        crate::state::state_for(&backend).render_cache.stats().pins,
        0
    );
}

// T21
/// Every attempt bad and every run failing on the link once more: at most
/// `(1 + transport_retries) x 2 x (1 + CALL_RETRIES)` requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_count_is_bounded() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_t21").await;
    let retries = 1usize;
    set_transport_retries(&kernel, &backend, retries as i64).await;
    // the endpoint names a default model: a failed hermes attempt restarts the
    // sequence under `default`, where the foreign tag fails again
    let mut passes = Vec::new();
    for run in 0..=retries {
        passes.push(pass(DONE));
        passes.push(pass(FOREIGN));
        passes.push(pass(FOREIGN));
        if run < retries {
            passes.push(vec![
                ScriptEvent::Token(FOREIGN.into()),
                ScriptEvent::TransportFail("connect failed".into()),
            ]);
        } else {
            passes.push(pass(FOREIGN));
        }
    }
    passes.extend((0..20).map(|_| pass(FOREIGN)));
    let s = script_as(&backend, PLAIN, passes);
    send_go(&kernel, &backend).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bound = (1 + retries) * 2 * (1 + crate::constants::CALL_RETRIES);
    assert!(
        s.passes_served() <= bound,
        "{} > {bound}",
        s.passes_served()
    );
    assert_eq!(
        s.passes_served(),
        (1 + retries) * (2 + crate::constants::CALL_RETRIES)
    );
}

// T22
/// The chat's `stop` or a cancel while an attempt streams drops the provider's stream
/// with the generation: the `complete` call ends with it (`RemoteProvider` drops the call
/// future and unwatches with its stream).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_cancels_complete() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t22").await;
    let steps = Steps::new(vec![
        Step::says(FOREIGN, PLAIN),
        Step {
            tokens: vec!["still ".into()],
            model: PLAIN,
            pending: true,
            transport: false,
        },
    ]);
    let run = generate(&kernel, &backend, &steps);
    let r = tokio::time::timeout(Duration::from_millis(500), run).await;
    assert!(r.is_err(), "the attempt never ends on its own");
    assert_eq!(steps.asked().len(), 2, "the re-ask was streaming");
    assert_eq!(steps.dropped(), 1, "its stream went with the generation");
    assert_eq!(
        crate::state::state_for(&backend).render_cache.stats().pins,
        0
    );
}

// T23
/// A failed attempt is read to its end (Q32): no token after the `Bad` reaches the
/// caller or the parts; the next request starts after the stream ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_attempt_read_unseen() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_t23").await;
    let mut first = pass("I will send.\n<invoke name=\"send\">");
    first.extend((0..500).map(|_| ScriptEvent::Token("\n</invoke".into())));
    let s = script_as(&backend, PLAIN, vec![first, pass(DONE)]);
    let mut rx = watch(&kernel, &backend).await;
    send_go(&kernel, &backend).await;
    let evs = events_until(&mut rx, ends_call).await;
    assert_eq!(s.passes_served(), 2);
    for v in &evs {
        let text = v["text"].as_str().unwrap_or_default();
        assert!(!text.contains("</invoke"), "{v}");
    }
    let reask = s.request_messages(1);
    let n = reask.len();
    assert_eq!(
        reask[n - 2],
        json!({"role": "assistant", "content": "I will send.\n<invoke name=\"send\">"})
    );
    assert!(
        reask[n - 1]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with('[')),
        "the report: {:?}",
        reask[n - 1]
    );
}

// R11
/// A good `default` turn is the turn of `4fd5bb4d`: for the five recorded answers the
/// caller's tokens, the parts and the stored row equal the recording
/// (`res/test/default_turn_*.txt`); the row without `attributes.utc_offset`, which the
/// stored offset (1.13) adds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_turn_same_as_before() {
    // (`prose` is gone from the list: an answer with no call is re-asked now, #795, so
    // its turn is not the recording's; the re-ask has its own units)
    for (name, recorded) in [
        (
            "reasoning",
            include_str!("model_format/res/test/default_turn_reasoning.txt"),
        ),
        (
            "one_call",
            include_str!("model_format/res/test/default_turn_one_call.txt"),
        ),
        (
            "several_calls",
            include_str!("model_format/res/test/default_turn_several_calls.txt"),
        ),
        (
            "repaired_opener",
            include_str!("model_format/res/test/default_turn_repaired_opener.txt"),
        ),
    ] {
        let body: String = recorded
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let want: Value = serde_json::from_str(&body).expect("a recording");
        let answer = want["answer"].as_str().expect("its answer");
        let got = rec::record_one(name, answer).await;
        assert_eq!(got["tokens"], want["tokens"], "{name}");
        assert_eq!(got["parts"], want["parts"], "{name}");
        // Two changes of the spec reach the row, and only those: `attributes.used` (W8)
        // where `4fd5bb4d` wrote no `usage` for a provider with no report, and the
        // how-to's Turns section, which teaches `stop` (W13), in the `verb_docs` bytes.
        let mut row = got["row"].clone();
        let used = row["attributes"]
            .as_object_mut()
            .and_then(|a| a.remove("used"));
        assert!(used.is_some(), "{name}: the row carries `used`");
        let mut want_row = want["row"].clone();
        let howto = crate::model_format::default::HOWTO_SEND;
        assert!(howto.contains(INQUIRY_WAIT), "{name}: the inquiry's wait line");
        let grown = i64::try_from(turns_of(howto).len()).unwrap()
            - i64::try_from(turns_of(BASE_TURNS).len()).unwrap()
            + i64::try_from(INQUIRY_WAIT.len()).unwrap()
            - i64::try_from(BASE_INQUIRY_WAIT.len()).unwrap();
        assert!(grown > 0, "{name}: the stop wording is in the how-to");
        let attrs = &mut want_row["attributes"];
        for p in attrs["parts"]["list"].as_array_mut().into_iter().flatten() {
            if p["part"] == json!("verb_docs") {
                p["bytes"] = json!(p["bytes"].as_i64().unwrap() + grown);
            }
        }
        if let Some(n) = attrs["totals"]["bytes"]["verb_docs"].as_i64() {
            attrs["totals"]["bytes"]["verb_docs"] = json!(n + grown);
        }
        // MAP-ONCE, pinned as bytes: the how-to opens with the environment section (150
        // bytes and its blank line: `MAP_ONCE_ENV`) and the context section names the
        // markers (`MAP_ONCE_CONTEXT` bytes more); the new input carries no time head
        // (`HEAD` bytes less). An edit to either text fails here.
        let mapped = MAP_ONCE_ENV + MAP_ONCE_CONTEXT;
        let head = HEAD;
        for p in attrs["parts"]["list"].as_array_mut().into_iter().flatten() {
            if p["part"] == json!("verb_docs") {
                p["bytes"] = json!(p["bytes"].as_i64().unwrap() + mapped);
            }
            if p["part"] == json!("new_input") {
                p["bytes"] = json!(p["bytes"].as_i64().unwrap() - head);
            }
        }
        for (part, by) in [("verb_docs", mapped), ("new_input", -head)] {
            if let Some(n) = attrs["totals"]["bytes"][part].as_i64() {
                attrs["totals"]["bytes"][part] = json!(n + by);
            }
        }
        assert_eq!(row, want_row, "{name}");
    }
}

/// MAP-ONCE's bytes in `verb_docs` for R11: the environment section, `## Environment` and
/// its one line with a 10-byte date and a 6-byte offset, then the blank line before the
/// next section.
const MAP_ONCE_ENV: i64 = 152;
/// MAP-ONCE's bytes in `verb_docs` for R11: what the context section's marker wording
/// added to it.
const MAP_ONCE_CONTEXT: i64 = 113;
/// The time head a row's text opened with before map-once: `[YYYY-MM-DD HH:MM ±HH:MM]\n`.
const HEAD: i64 = 26;

/// The Turns section of the how-to on `4fd5bb4d` (`assembly.rs` `SEND_TO_HOWTO`), byte
/// for byte: R11's measure of what W13 added.
const BASE_TURNS: &str = r#"## Turns
Only your ANSWER acts: a `<call>` in your thinking does not run; write it in your answer,
after thinking; every call in it runs. End every turn with a call, with
`<call name="await"></call>` (stop until the next message wakes you; what you wait for is in
flight, not lost, and a job from `exec start` wakes you with `job_done`; refused when nothing
is pending) or with `<call name="done"></call>` (the work is done, checked as under Working). A turn with
no call is rerolled.
"#;

/// The inquiry's wait, as the how-to on `4fd5bb4d` wrote it, and as it reads now that
/// a pass ends with `done` (#795).
const BASE_INQUIRY_WAIT: &str = "work on, or `<call name=\"await\"></call>`\nfor it.";
const INQUIRY_WAIT: &str = "work on, or end your answer with\n`<call name=\"done\"></call>` until it comes.";

/// The section of a how-to on ending an answer (`## Turns` on `4fd5bb4d`): from its
/// heading to the next one.
fn turns_of(howto: &str) -> &str {
    let at = howto
        .find("## Turns")
        .or_else(|| howto.find("## Ending an answer"))
        .expect("a section on ending an answer");
    let rest = &howto[at..];
    // through the newline that ends its last line, the blank line before the next aside
    let end = rest[2..].find("\n## ").map_or(rest.len(), |i| i + 2);
    &rest[..end]
}

/// The harness of the `4fd5bb4d` recording (`res/test/default_turn_*.txt`), unchanged:
/// a chat `rec_ai_<name>` on a provider that streams the answer in 4-char chunks and
/// names the model `stub`, a call target `notes`, both watched.
mod rec {
    use super::*;
    use fantastic_kernel::test_support::{as_agent, grant};

    const REC: &str = "rec_backend.tools";

    type Queues = std::collections::HashMap<String, Arc<std::sync::Mutex<Vec<String>>>>;

    static REC_SCRIPTS: std::sync::OnceLock<std::sync::Mutex<Queues>> = std::sync::OnceLock::new();

    fn rec_scripts() -> std::sync::MutexGuard<'static, Queues> {
        REC_SCRIPTS
            .get_or_init(|| std::sync::Mutex::new(Queues::new()))
            .lock()
            .unwrap()
    }

    /// Streams each scripted pass in 4-char chunks, no delay; unscripted passes are DONE.
    struct RecProvider(Arc<std::sync::Mutex<Vec<String>>>);

    #[async_trait]
    impl Provider for RecProvider {
        async fn chat(
            &self,
            _messages: &[Value],
            _max_tokens: Option<i64>,
        ) -> Result<ProviderStream, ProviderError> {
            let text = {
                let mut q = self.0.lock().unwrap();
                if q.is_empty() {
                    DONE.to_string()
                } else {
                    q.remove(0)
                }
            };
            let chars: Vec<char> = text.chars().collect();
            let chunks: Vec<String> = chars.chunks(4).map(|c| c.iter().collect()).collect();
            Ok(Box::pin(
                futures_util::stream::iter(chunks).map(|c| Ok(ProviderEvent::Token(c))),
            ))
        }
        fn model(&self) -> String {
            "stub".into()
        }
    }

    struct RecBundle;

    #[async_trait]
    impl Handler for RecBundle {
        crate::tests_decl::chat_decls!();
        fn name(&self) -> &str {
            "rec_backend"
        }
        async fn handle(
            &self,
            agent_id: &AgentId,
            call: Self::Call,
            kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            let script = Arc::clone(
                rec_scripts()
                    .entry(agent_id.as_str().to_string())
                    .or_default(),
            );
            let reply = verbs::handle(
                agent_id,
                call,
                kernel,
                CFG,
                move |_client| Arc::new(RecProvider(Arc::clone(&script))) as Arc<dyn Provider>,
                || json!({"id": "rec_backend"}),
            )
            .await?;
            Ok(Some(reply))
        }
    }

    async fn op(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
        kernel
            .operator_send(vec![Item::new(AgentId::from(target), payload)])
            .await
            .remove(0)
            .result
    }

    /// Volatile keys whose values are replaced by `"<key>"`.
    const VOLATILE: &[&str] = &[
        "ts",
        "id",
        "parent",
        "stream_id",
        "re",
        "hash",
        "task_id",
        "client_id",
        "digest",
    ];

    fn normalise(v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, x)| {
                        if VOLATILE.contains(&k.as_str()) && !x.is_null() {
                            (k.clone(), json!(format!("<{k}>")))
                        } else {
                            (k.clone(), normalise(x))
                        }
                    })
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(normalise).collect()),
            other => other.clone(),
        }
    }

    /// Parts as `{start, text (deltas joined), end}`, in start order.
    fn parts_of(evs: &[Value]) -> Vec<Value> {
        let mut order: Vec<String> = Vec::new();
        let mut by: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
        for e in evs {
            let t = e["type"].as_str().unwrap_or_default();
            if !t.starts_with("part_") {
                continue;
            }
            let sid = e["stream_id"].as_str().unwrap_or_default().to_string();
            let entry = by.entry(sid.clone()).or_insert_with(|| {
                order.push(sid.clone());
                json!({"start": null, "text": "", "end": null})
            });
            match t {
                "part_start" => entry["start"] = normalise(e),
                "part_delta" => {
                    let mut s = entry["text"].as_str().unwrap_or_default().to_string();
                    s.push_str(e["text"].as_str().unwrap_or_default());
                    entry["text"] = json!(s);
                }
                _ => entry["end"] = normalise(e),
            }
        }
        order.into_iter().map(|s| by.remove(&s).unwrap()).collect()
    }

    /// `{tokens, parts, row}` of `answer`'s turn, as the recording took them.
    pub(super) async fn record_one(name: &str, answer: &str) -> Value {
        let tmp = TempDir::new().unwrap();
        let kernel = base_kernel(&tmp, |k| k.bundles.register(REC, RecBundle)).await;
        let ai_id = format!("rec_ai_{name}");
        let memory = fantastic_ram_store::test_support::memory_meta(
            &kernel,
            &ai_id,
            json!({"cli": {"call": ["*"]}}),
        )
        .await;
        let mut create = json!({
            "type": "create_agent",
            "parent_id": "kernel",
            "handler_module": REC,
            "id": ai_id,
            "model": "stub",
            "context_window": 32768,
        });
        for (k, v) in memory {
            create[k] = v;
        }
        let r = op(&kernel, "kernel_state", create).await;
        assert_eq!(r["id"], json!(ai_id), "{r}");
        let r = op(
            &kernel,
            "kernel_state",
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": fantastic_ram_store::HANDLER_MODULE, "id": "notes"}),
        )
        .await;
        assert_eq!(r["id"], json!("notes"), "{r}");
        grant(&kernel, &ai_id, "notes", json!({"call": ["*"]}));
        let mut ai_rx = kernel.ensure_inbox(&AgentId::from("w_ai"));
        assert!(fantastic_kernel::send::as_operator(
            kernel.watch(&AgentId::from(ai_id.as_str()), AgentId::from("w_ai"))
        )
        .await
        .is_none());
        let mut notes_rx = kernel.ensure_inbox(&AgentId::from("w_notes"));
        assert!(fantastic_kernel::send::as_operator(
            kernel.watch(&AgentId::from("notes"), AgentId::from("w_notes"))
        )
        .await
        .is_none());
        *rec_scripts()
            .entry(ai_id.clone())
            .or_default()
            .lock()
            .unwrap() = vec![answer.to_string()];
        let _ = op(
            &kernel,
            &ai_id,
            json!({"type": "send", "text": "go", "client_id": "rec"}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let mut ai_evs = Vec::new();
        while let Some(v) = ai_rx.try_recv() {
            ai_evs.push(v.payload);
        }
        let end = ai_evs
            .iter()
            .position(|v| {
                v["type"] == "status"
                    && ["done", "failed", "stalled", "awaiting"]
                        .contains(&v["phase"].as_str().unwrap_or_default())
            })
            .map_or(ai_evs.len(), |i| i + 1);
        let first: Vec<Value> = ai_evs[..end].to_vec();
        let tokens: Vec<Value> = first
            .iter()
            .filter(|v| v["type"] == "token")
            .map(|v| v["text"].clone())
            .collect();
        let mut notes_evs = Vec::new();
        while let Some(v) = notes_rx.try_recv() {
            notes_evs.push(v.payload);
        }
        let mut all_parts: Vec<Value> = first.clone();
        all_parts.extend(notes_evs.iter().cloned());
        let parts = parts_of(&all_parts);
        let res = as_agent(
            AgentId::from(ai_id.as_str()),
            crate::memory::store_call(
                &AgentId::from(ai_id.as_str()),
                &kernel,
                json!({"type": "read"}),
            ),
        )
        .await
        .expect("store read");
        let mut row = res["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["role"] == "assistant")
            .cloned()
            .expect("an assistant row");
        if let Some(a) = row["attributes"].as_object_mut() {
            a.remove(crate::row_attrs::UTC_OFFSET);
        }
        json!({"tokens": tokens, "parts": parts, "row": normalise(&row)})
    }
}

// M16
/// A failed attempt whose reply names a model of another type: no re-ask pair from it;
/// the sequence restarts once under the new type, and the restarted request holds no
/// call form of the old type (both directions). A second such reply does not restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_type_restarts_once() {
    let tmp = TempDir::new().unwrap();
    // hermes -> default
    let (kernel, backend) = mk_hermes(&tmp, "tr_m16a").await;
    let steps = Steps::new(vec![Step::says(DONE, PLAIN), Step::says(DONE, PLAIN)]);
    let (text, _) = answered(generate(&kernel, &backend, &steps).await);
    assert_eq!(text, DONE);
    let asked = steps.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1].len(), asked[0].len(), "no re-ask pair");
    let restarted = Value::Array(asked[1].clone()).to_string();
    assert!(!restarted.contains("<tool_call>"), "{restarted}");
    assert!(!restarted.contains("</tool_call>"), "{restarted}");
    let sw = switches(&kernel, &backend).await;
    assert_eq!(sw.len(), 1, "{sw:?}");
    assert_eq!(
        (&sw[0]["from"], &sw[0]["to"]),
        (&json!(HERMES), &json!("default"))
    );

    // default -> hermes
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tr_m16b").await;
    let steps = Steps::new(vec![
        Step::says(FOREIGN, QWEN),
        Step::says(&hermes(DONE), QWEN),
    ]);
    let (text, _) = answered(generate(&kernel, &backend, &steps).await);
    // the hermes reader writes the call as `render_call` does (1.4)
    assert_eq!(
        text,
        crate::tool_parse::render_call("done", &serde_json::Map::new())
    );
    let asked = steps.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1].len(), asked[0].len(), "no re-ask pair");
    let restarted = Value::Array(asked[1].clone()).to_string();
    assert!(!restarted.contains("<call name="), "{restarted}");
    let sw = switches(&kernel, &backend).await;
    assert_eq!(sw.len(), 1, "{sw:?}");
    assert_eq!(
        (&sw[0]["from"], &sw[0]["to"]),
        (&json!("default"), &json!(HERMES))
    );

    // a second such reply in the same turn does not restart again
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_m16c").await;
    let steps = Steps::new(vec![
        Step::says(DONE, PLAIN),
        Step::says(FOREIGN, QWEN),
        Step::says(DONE, QWEN),
    ]);
    let (text, _) = answered(generate(&kernel, &backend, &steps).await);
    assert_eq!(text, DONE);
    let asked = steps.asked();
    assert_eq!(asked.len(), 3);
    assert_eq!(
        asked[2].len(),
        asked[1].len() + 2,
        "a re-ask, not a restart"
    );
    assert_eq!(asked[2][..asked[1].len()], asked[1][..]);
    assert_eq!(
        switches(&kernel, &backend).await.len(),
        1,
        "one restart traced"
    );
}

// K36
/// A failed attempt and its re-ask do not touch the cache: its entries equal those of a
/// pass with no retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_leaves_cache() {
    let mut stats = Vec::new();
    for (tag, retry) in [("tr_k36a", false), ("tr_k36b", true)] {
        let tmp = TempDir::new().unwrap();
        let (kernel, backend) = mk_hermes(&tmp, tag).await;
        let first = Steps::new(vec![Step::says(&hermes(DONE), QWEN)]);
        answered(generate(&kernel, &backend, &first).await);
        let mut second = Vec::new();
        if retry {
            second.push(Step::says(DONE, QWEN));
        }
        second.push(Step::says(&hermes(DONE), QWEN));
        let second = Steps::new(second);
        answered(generate(&kernel, &backend, &second).await);
        let st = crate::state::state_for(&backend).render_cache.stats();
        assert_eq!(st.pins, 0, "{tag}");
        stats.push((st.entries, st.bytes, st.counted));
    }
    assert_eq!(stats[0], stats[1]);
}

// K37
/// A cancelled pass: the entries it inserted stay and equal a fresh render of their
/// rows; the kept chain is the last finished pass's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_keeps_pure_entries() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_k37").await;
    let first = Steps::new(vec![Step::says(&hermes(DONE), QWEN)]);
    answered(generate(&kernel, &backend, &first).await);
    let state = crate::state::state_for(&backend);
    let cache = &state.render_cache;
    let chain = cache.chain(HERMES);
    let hanging = Steps::new(vec![Step {
        tokens: vec!["thinking it over".into()],
        model: QWEN,
        pending: true,
        transport: false,
    }]);
    let r = tokio::time::timeout(
        Duration::from_millis(400),
        generate(&kernel, &backend, &hanging),
    )
    .await;
    assert!(r.is_err(), "the pass was cancelled");
    assert_eq!(
        cache.chain(HERMES),
        chain,
        "the kept chain is the finished pass's"
    );
    let st = cache.stats();
    assert_eq!(st.pins, 0);
    let rows = store_rows(&kernel, &backend).await;
    for (kind, id) in &st.keys {
        let row = rows
            .iter()
            .find(|r| r["id"].as_i64() == Some(*id))
            .expect("an entry names a stored row");
        let fresh = Hermes.render(row["content"].as_str().unwrap_or(""));
        assert_eq!(cache.rendered(kind, *id), Some(fresh), "row {id}");
    }
}

// K41
/// A pass cancelled, failed on the link, or ended by the harness stop while it holds
/// pins: none is left once its guard dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pins_released_on_drop() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_hermes(&tmp, "tr_k41").await;
    let first = Steps::new(vec![Step::says(&hermes(DONE), QWEN)]);
    answered(generate(&kernel, &backend, &first).await);
    let pins = || crate::state::state_for(&backend).render_cache.stats().pins;

    // cancelled
    let hanging = Steps::new(vec![Step {
        tokens: vec![],
        model: QWEN,
        pending: true,
        transport: false,
    }]);
    let r = tokio::time::timeout(
        Duration::from_millis(300),
        generate(&kernel, &backend, &hanging),
    )
    .await;
    assert!(r.is_err());
    assert_eq!(pins(), 0, "cancelled");

    // failed on the link
    let link = Steps::new(vec![Step {
        tokens: vec!["par".into()],
        model: QWEN,
        pending: false,
        transport: true,
    }]);
    assert!(matches!(
        generate(&kernel, &backend, &link).await,
        Err(GenFailure::Provider(_))
    ));
    assert_eq!(pins(), 0, "link failure");

    // the harness stop
    let bad = Steps::new((0..4).map(|_| Step::says(DONE, QWEN)).collect());
    assert!(matches!(
        generate(&kernel, &backend, &bad).await,
        Ok(Generation::Stopped { .. })
    ));
    assert_eq!(pins(), 0, "harness stop");
}

// ─── the cases of the old stream checker (#499), same fixtures ──────────────

/// `text <invoke name="send">` then thousands of looping tokens: the attempt fails at
/// the tag, the rest is read unseen, nothing of it is stored, the report reaches the
/// re-ask, and the turn ends in a blink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_looping_foreign_tag_is_reasked() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_loop").await;
    let mut first = pass("I will send.\n<invoke name=\"send\">");
    first.extend((0..5000).map(|_| ScriptEvent::Token("\n</invoke".into())));
    let s = script(
        &backend,
        vec![first, pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let started = std::time::Instant::now();
    send_go(&kernel, &backend).await;
    assert!(
        until_passes(&s, 2).await,
        "the re-ask never reached the model"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    let rows = answers(&kernel, &backend).await;
    assert_eq!(rows, vec![DONE.to_string()], "{rows:?}");
    let seen = s.request(1);
    assert!(
        seen.contains("I will send.\\n<invoke name=\\\"send\\\">"),
        "{seen}"
    );
    assert!(!seen.contains("</invoke"), "{seen}");
}

/// A correct call is never failed, even delivered a char at a time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_correct_call_stream_is_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_ok").await;
    let text = "Done soon.\n<call name=\"done\"></call>";
    let s = script(&backend, vec![chars(text)], Duration::ZERO);
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 1).await);
    let rows = answers(&kernel, &backend).await;
    assert_eq!(rows[0], text, "{rows:?}");
    assert_eq!(s.passes_served(), 1);
}

/// A quoted complete call inside an argument is text, not foreign (#543).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quoted_call_in_an_argument_is_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_quoted").await;
    let text = "<call name=\"send\"><routing><target_id>probe</target_id></routing><payload><type>say</type>\
                <text>end with <call name=\"done\"></call> then</text></payload></call>";
    let s = script(
        &backend,
        vec![chars(text), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 1).await);
    let rows = answers(&kernel, &backend).await;
    assert_eq!(rows[0], text, "{rows:?}");
}

/// A foreign tag inside thinking is not failed (thinking is excluded); prose that only
/// mentions the word invoke is not failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thinking_and_prose_are_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_think").await;
    let text = "<think>maybe <invoke name=\"send\"> is it</think>I should invoke the done call.\n\
                <call name=\"done\"></call>";
    let s = script(&backend, vec![chars(text)], Duration::ZERO);
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 1).await);
    let rows = answers(&kernel, &backend).await;
    assert_eq!(
        rows[0], "I should invoke the done call.\n<call name=\"done\"></call>",
        "{rows:?}"
    );
    assert_eq!(s.passes_served(), 1);
}

/// A call that lost its `<` is not failed: it runs and is stored whole (Q1 B).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_that_lost_its_lt_is_not_cut() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_cut_lt").await;
    let good = call_envelope("kernel", json!({"type": "reflect", "tree": "none"}), None);
    let s = script(
        &backend,
        vec![
            chars(&format!("<think>x</think>{}", &good[1..])),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    send_go(&kernel, &backend).await;
    assert!(until_passes(&s, 2).await);
    let seen = s.request(1);
    assert!(!seen.contains("[malformed call"), "{seen}");
    assert!(seen.contains("kernel reflect"), "{seen}");
    let rows = answers(&kernel, &backend).await;
    assert!(rows[0].starts_with("<call name=\"send\">"), "{rows:?}");
}

/// The harness stop reaches the sender: a peer's `inquiry` to a chat whose every attempt
/// is foreign is answered with the stop's reason (`close_inquiry`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_harness_stop_is_told_to_the_asker() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_cut_ask").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let ask = call_envelope(a.as_str(), json!({"type": "inquiry", "text": "go"}), None);
    let fb = script(
        &b,
        vec![
            pass(ask),
            pass(r#"<call name="done"></call>"#),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let s = script(&a, (0..12).map(|_| pass(FOREIGN)).collect(), Duration::ZERO);
    let _ = kernel
        .operator_send(vec![Item::new(
            b.clone(),
            json!({"type": "send", "text": "ask", "client_id": "probe"}),
        )])
        .await;
    assert!(until_passes(&fb, 3).await, "the asker never got the answer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = 1 + crate::constants::CALL_RETRIES;
    assert_eq!(s.passes_served(), n);
    let reason = crate::model_format::fill(
        crate::model_format::default::STOP_REASON,
        &[("n", &n.to_string())],
    );
    let seen = fb.request(2);
    assert!(seen.contains(&reason), "{seen}");
}
