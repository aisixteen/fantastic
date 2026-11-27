//! Token accounting, chat-side (#766, spec 1.7 and 2.8): `attributes.used` `{pass,
//! running}` with `{turn: {accepted, rejected}, task, total}` of four counts each, and
//! `attributes.window`, attempt 1's prompt side. Every case drives the real pass path
//! (`run_generation`, the task pass) over a provider that reports per request, like the
//! remote provider: it clears the prompt and cache counts before each request
//! (`clear_prompt_tokens`), never the completion count, and writes the reply's counts
//! only when the stream ended whole.
//!
//! U13 (the provider's reply keys unchanged) is the existing `tests_remote_provider.rs`,
//! unedited.

use super::*;
use crate::agent_loop::{run_generation, GenFailure, Generation};
use crate::model_format::pass::Pass;
use crate::provider::ProviderReport;
use futures_util::StreamExt;

const DONE: &str = r#"<call name="done"></call>"#;
/// A foreign call tag: the `default` reader fails the attempt at it.
const FOREIGN: &str = "Sending.\n<invoke name=\"send\">";
/// A model id of the `default` type.
const PLAIN: &str = "test-model";
/// A model id of the `hermes` type.
const QWEN: &str = "qwen3-8b";

/// The endpoint's reply counts: `prompt_tokens`, `completion_tokens` and the cache
/// counts when it has them.
fn rep(
    prompt: i64,
    completion: i64,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
) -> ProviderReport {
    ProviderReport {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        stop_reason: None,
        cache_read,
        cache_write,
    }
}

/// A reply that carries no usage at all.
fn no_usage() -> ProviderReport {
    ProviderReport::default()
}

/// Four counts as a row keeps them.
fn c(prompt: i64, completion: i64, cache_read: i64, cache_write: i64) -> Value {
    json!({"prompt": prompt, "completion": completion,
           "cache_read": cache_read, "cache_write": cache_write})
}

/// Nothing counted.
fn zero() -> Value {
    c(0, 0, 0, 0)
}

/// The four counts of two values, added.
fn add(a: &Value, b: &Value) -> Value {
    let n = |v: &Value, k: &str| v[k].as_i64().unwrap_or(0);
    c(
        n(a, "prompt") + n(b, "prompt"),
        n(a, "completion") + n(b, "completion"),
        n(a, "cache_read") + n(b, "cache_read"),
        n(a, "cache_write") + n(b, "cache_write"),
    )
}

/// The four counts added up.
fn total_of(v: &Value) -> i64 {
    ["prompt", "completion", "cache_read", "cache_write"]
        .iter()
        .map(|k| v[*k].as_i64().unwrap_or(0))
        .sum()
}

/// One scripted request.
struct Step {
    text: String,
    /// The model the endpoint names for it; empty keeps the last one.
    model: &'static str,
    /// The reply's counts, written when the stream ends whole; `None` writes nothing
    /// (what the endpoint left before stays).
    report: Option<ProviderReport>,
    /// After its text the link fails.
    transport: bool,
    /// After its text the stream stays open: only a drop ends it.
    pending: bool,
}

impl Step {
    fn says(text: &str, report: ProviderReport) -> Self {
        Self {
            text: text.to_string(),
            model: "",
            report: Some(report),
            transport: false,
            pending: false,
        }
    }

    fn named_model(mut self, model: &'static str) -> Self {
        self.model = model;
        self
    }

    fn unsettled(text: &str) -> Self {
        Self {
            text: text.to_string(),
            model: "",
            report: None,
            transport: false,
            pending: false,
        }
    }

    fn link_fails() -> Self {
        Self {
            transport: true,
            ..Self::unsettled("")
        }
    }

    fn hangs() -> Self {
        Self {
            pending: true,
            ..Self::unsettled("")
        }
    }
}

/// A provider that reports per request, as the remote provider does.
struct Metered {
    steps: std::sync::Mutex<std::collections::VecDeque<Step>>,
    asked: std::sync::Mutex<Vec<Vec<Value>>>,
    model: std::sync::Mutex<String>,
    report: Arc<std::sync::Mutex<ProviderReport>>,
}

impl Metered {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: std::sync::Mutex::new(steps.into_iter().collect()),
            asked: std::sync::Mutex::new(Vec::new()),
            model: std::sync::Mutex::new(PLAIN.to_string()),
            report: Arc::new(std::sync::Mutex::new(ProviderReport::default())),
        })
    }

    fn asked(&self) -> Vec<Vec<Value>> {
        self.asked.lock().unwrap().clone()
    }

    fn left(&self) -> usize {
        self.steps.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for Metered {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.asked.lock().unwrap().push(messages.to_vec());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Step::says(DONE, no_usage()));
        if !step.model.is_empty() {
            *self.model.lock().unwrap() = step.model.to_string();
        }
        let mut evs: Vec<Result<ProviderEvent, ProviderError>> = Vec::new();
        if !step.text.is_empty() {
            evs.push(Ok(ProviderEvent::Token(step.text.clone())));
        }
        let head = futures_util::stream::iter(evs);
        if step.transport {
            let fail =
                futures_util::stream::iter(vec![Err(ProviderError::transport("link dropped"))]);
            return Ok(Box::pin(head.chain(fail)));
        }
        if step.pending {
            return Ok(Box::pin(head.chain(futures_util::stream::pending())));
        }
        // the reply settles the report once the stream ended whole
        let cell = Arc::clone(&self.report);
        let settle = step.report;
        let tail = futures_util::stream::once(async move {
            if let Some(r) = settle {
                *cell.lock().unwrap() = r;
            }
        })
        .filter_map(|()| futures_util::future::ready(None::<Result<ProviderEvent, ProviderError>>));
        Ok(Box::pin(head.chain(tail)))
    }
    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }
    fn report(&self) -> ProviderReport {
        self.report.lock().unwrap().clone()
    }
    fn clear_prompt_tokens(&self) {
        let mut r = self.report.lock().unwrap();
        r.prompt_tokens = None;
        r.cache_read = None;
        r.cache_write = None;
    }
    fn clear_stop_reason(&self) {
        self.report.lock().unwrap().stop_reason = None;
    }
}

/// One generation of `agent` on `provider`, run as the agent, as `send` runs it.
async fn generate(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
    provider: &Arc<Metered>,
) -> Result<Generation, GenFailure> {
    let p: Arc<dyn Provider> = Arc::clone(provider) as Arc<dyn Provider>;
    let state = crate::state::state_for(agent);
    fantastic_kernel::test_support::as_agent(
        agent.clone(),
        run_generation(&p, agent, &state, "go", kernel, "probe", CFG),
    )
    .await
}

/// A generation that answered; panics on anything else.
async fn answers(kernel: &Arc<Kernel>, agent: &AgentId, provider: &Arc<Metered>) {
    match generate(kernel, agent, provider).await {
        Ok(Generation::Answered { .. }) => {}
        Ok(Generation::Stopped { reason, .. }) => panic!("stopped: {reason}"),
        Err(_) => panic!("the generation failed"),
    }
}

/// A task pass of `agent`'s chat outside a turn: the summarizer over one row.
async fn summarize(kernel: &Arc<Kernel>, agent: &AgentId, provider: &Arc<Metered>) -> String {
    let p: Arc<dyn Provider> = Arc::clone(provider) as Arc<dyn Provider>;
    let state = crate::state::state_for(agent);
    let rows = vec![json!({"role": "user", "id": 1, "content": "go"})];
    fantastic_kernel::test_support::as_agent(agent.clone(), async {
        let pass = Pass::new(p, agent, &state, kernel).await;
        crate::model_format::pass::settled(&pass, Some(kernel), |p| crate::strategies::summarize(p, &rows, None)).await.0
    })
    .await
}

/// Every assistant row the agent's store holds, whole, in id order.
async fn assistant_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["role"] == json!("assistant"))
        .collect()
}

/// The newest assistant row.
async fn last_row(kernel: &Arc<Kernel>, agent: &AgentId) -> Value {
    assistant_rows(kernel, agent)
        .await
        .pop()
        .expect("an assistant row")
}

/// `attributes.used.pass` of a row.
fn pass_of(row: &Value) -> &Value {
    &row["attributes"]["used"]["pass"]
}

/// `attributes.used.running` of a row.
fn running_of(row: &Value) -> &Value {
    &row["attributes"]["used"]["running"]
}

/// Set a chat's record `model`.
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

/// A watcher on `agent`'s events.
async fn watch(kernel: &Arc<Kernel>, agent: &AgentId) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from(format!("w_{}", agent.as_str()).as_str());
    let rx = kernel.ensure_inbox(&w);
    assert!(fantastic_kernel::send::as_operator(kernel.watch(agent, w))
        .await
        .is_none());
    rx
}

/// The detail of the first `done` status on `rx`, waited for at most 5 s.
async fn done_status(rx: &mut fantastic_kernel::inbox::InboxReceiver) -> Value {
    for _ in 0..200 {
        while let Some(v) = rx.try_recv() {
            if v.payload["type"] == json!("status") && v.payload["phase"] == json!("done") {
                // the status's keys are its `detail`
                return v.payload["detail"].clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no done status");
}

/// The chat's calibration.
fn calib(agent: &AgentId) -> Option<f64> {
    *crate::state::state_for(agent).calib.lock().unwrap()
}

// U1
/// One attempt that reports: its counts are `turn.accepted`, nothing else is counted,
/// and the window is its prompt side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_attempt() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_one").await;
    let p = Metered::new(vec![Step::says(DONE, rep(1000, 40, Some(600), Some(300)))]);
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    let pass = pass_of(&row);
    assert_eq!(pass["turn"]["accepted"], c(100, 40, 600, 300), "{row}");
    assert_eq!(pass["turn"]["rejected"], zero());
    assert_eq!(pass["task"], zero());
    assert_eq!(pass["total"], pass["turn"]["accepted"]);
    assert_eq!(
        row["attributes"]["window"],
        json!({"prompt": 100, "cache_read": 600, "cache_write": 300})
    );
}

// U2
/// Three attempts, the third accepted: the first two are `turn.rejected`, the window is
/// attempt 1's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_split() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_split").await;
    let p = Metered::new(vec![
        Step::says(FOREIGN, rep(1000, 30, Some(200), None)),
        Step::says(FOREIGN, rep(1100, 35, Some(900), None)),
        Step::says(DONE, rep(1200, 7, Some(1000), None)),
    ]);
    answers(&kernel, &a, &p).await;
    assert_eq!(p.asked().len(), 3);
    let row = last_row(&kernel, &a).await;
    let pass = pass_of(&row);
    assert_eq!(
        pass["turn"]["rejected"],
        add(&c(800, 30, 200, 0), &c(200, 35, 900, 0))
    );
    assert_eq!(pass["turn"]["accepted"], c(200, 7, 1000, 0));
    assert_eq!(pass["task"], zero());
    assert_eq!(
        pass["total"],
        add(&pass["turn"]["rejected"], &pass["turn"]["accepted"])
    );
    assert_eq!(
        row["attributes"]["window"],
        json!({"prompt": 800, "cache_read": 200})
    );
}

// U3
/// A turn the harness stopped: nothing accepted, every attempt rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_stop_accepts_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_stop").await;
    let n = 1 + crate::constants::CALL_RETRIES;
    let p = Metered::new(
        (0..n)
            .map(|i| Step::says(FOREIGN, rep(100 + i as i64, 10, None, None)))
            .collect(),
    );
    let g = generate(&kernel, &a, &p).await;
    assert!(matches!(g, Ok(Generation::Stopped { .. })), "not stopped");
    let row = last_row(&kernel, &a).await;
    let pass = pass_of(&row);
    assert_eq!(pass["turn"]["accepted"], zero(), "{row}");
    let mut want = zero();
    for i in 0..n {
        want = add(&want, &c(100 + i as i64, 10, 0, 0));
    }
    assert_eq!(pass["turn"]["rejected"], want);
    assert_eq!(pass["total"], want);
    // the stopped row carries the window too: attempt 1's
    assert_eq!(row["attributes"]["window"], json!({"prompt": 100}));
}

// U4
/// A request whose reply never came adds nothing: a link failure, and a stream that
/// ended with no reply counts written, where the completion count an earlier request
/// left still stands in the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_report_no_stale_count() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_stale").await;
    let p = Metered::new(vec![
        Step::says(DONE, rep(500, 99, None, None)),
        // the link fails: the turn fails, nothing of it is read
        Step::link_fails(),
        // a whole stream whose reply wrote nothing: completion 99 is still there
        Step::unsettled(DONE),
    ]);
    answers(&kernel, &a, &p).await;
    assert!(generate(&kernel, &a, &p).await.is_err(), "the link failed");
    answers(&kernel, &a, &p).await;
    assert_eq!(
        p.report().completion_tokens,
        Some(99),
        "the stale count stands"
    );
    let rows = assistant_rows(&kernel, &a).await;
    assert_eq!(rows.len(), 2);
    let pass = pass_of(&rows[1]);
    assert_eq!(pass["total"], zero(), "{}", rows[1]);
    assert!(rows[1]["attributes"].get("window").is_none());
    assert_eq!(
        running_of(&rows[1])["total"],
        c(500, 99, 0, 0),
        "the running sum holds only the reported request"
    );
}

// U5
/// An endpoint with no cache counts: 0 in `used`, absent from the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_cache_counts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_nocache").await;
    let p = Metered::new(vec![Step::says(DONE, rep(400, 12, None, None))]);
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    assert_eq!(pass_of(&row)["turn"]["accepted"], c(400, 12, 0, 0));
    assert_eq!(row["attributes"]["window"], json!({"prompt": 400}));
}

// U6
/// Five turns, two of them with task passes (one inside the turn, one outside): each
/// row's `running` is the sum of every `pass` so far, per part and per count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_is_the_sum() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_sum").await;
    let state = crate::state::state_for(&a);
    let p = Metered::new(vec![
        // 1: a retry
        Step::says(FOREIGN, rep(100, 10, Some(5), None)),
        Step::says(DONE, rep(110, 11, Some(6), None)),
        // 2: plain
        Step::says(DONE, rep(120, 12, None, Some(7))),
        // a task pass outside a turn, then 3
        Step::says("SUMMARY", rep(50, 5, None, None)),
        Step::says(DONE, rep(130, 13, None, None)),
        // 4: a recall owed, so a task pass inside the turn first
        Step::says("{}", rep(60, 6, None, None)),
        Step::says(DONE, rep(140, 14, Some(1), Some(1))),
        // 5
        Step::says(DONE, rep(150, 15, None, None)),
    ]);
    answers(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    assert_eq!(summarize(&kernel, &a, &p).await, "SUMMARY");
    answers(&kernel, &a, &p).await;
    *state.recall_due.lock().unwrap() = true;
    answers(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    assert_eq!(p.left(), 0, "every scripted request was made");
    let rows = assistant_rows(&kernel, &a).await;
    assert_eq!(rows.len(), 5);
    let mut sum = json!({"accepted": zero(), "rejected": zero(), "task": zero(), "total": zero()});
    for row in &rows {
        let pass = pass_of(row);
        sum = json!({
            "accepted": add(&sum["accepted"], &pass["turn"]["accepted"]),
            "rejected": add(&sum["rejected"], &pass["turn"]["rejected"]),
            "task": add(&sum["task"], &pass["task"]),
            "total": add(&sum["total"], &pass["total"]),
        });
        let running = running_of(row);
        assert_eq!(running["turn"]["accepted"], sum["accepted"], "{row}");
        assert_eq!(running["turn"]["rejected"], sum["rejected"], "{row}");
        assert_eq!(running["task"], sum["task"], "{row}");
        assert_eq!(running["total"], sum["total"], "{row}");
    }
    assert_eq!(pass_of(&rows[2])["task"], c(50, 5, 0, 0));
    assert_eq!(pass_of(&rows[3])["task"], c(60, 6, 0, 0));
    assert_eq!(running_of(&rows[4])["task"], c(110, 11, 0, 0));
}

// U7
/// A restart: the state is new, and `running` goes on from the newest row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_survives_restart() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_restart").await;
    let p = Metered::new(vec![
        Step::says(DONE, rep(100, 10, None, None)),
        Step::says(DONE, rep(200, 20, None, None)),
        Step::says(DONE, rep(300, 30, None, None)),
    ]);
    answers(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    crate::state::drop_state(&a);
    answers(&kernel, &a, &p).await;
    let rows = assistant_rows(&kernel, &a).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(
        running_of(&rows[2])["total"],
        add(&running_of(&rows[1])["total"], &pass_of(&rows[2])["total"])
    );
    assert_eq!(running_of(&rows[2])["total"], c(600, 60, 0, 0));
}

// U8
/// A thinking-only attempt, a link failure after a reported attempt and a cancelled turn store no
/// assistant row: their counts are `turn.rejected` of the next row's pass, and the sum
/// over the rows is still the running sum.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_lands_on_next_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_pending").await;
    let p = Metered::new(vec![
        Step::says(DONE, rep(10, 1, None, None)),
        // a thinking-only attempt and a reported bad attempt, then the link fails
        Step::says("<think>hmm</think>", rep(20, 2, None, None)),
        Step::says(FOREIGN, rep(30, 3, None, None)),
        Step::link_fails(),
        // a reported bad attempt, then the turn is cancelled mid-stream
        Step::says(FOREIGN, rep(40, 4, None, None)),
        Step::hangs(),
        // the next stored row
        Step::says(DONE, rep(50, 5, None, None)),
    ]);
    answers(&kernel, &a, &p).await;
    assert!(generate(&kernel, &a, &p).await.is_err(), "the link failed");
    let cancelled =
        tokio::time::timeout(Duration::from_millis(500), generate(&kernel, &a, &p)).await;
    assert!(cancelled.is_err(), "the turn was cancelled");
    answers(&kernel, &a, &p).await;
    let rows = assistant_rows(&kernel, &a).await;
    assert_eq!(rows.len(), 2);
    let pass = pass_of(&rows[1]);
    assert_eq!(pass["turn"]["accepted"], c(50, 5, 0, 0));
    assert_eq!(pass["turn"]["rejected"], c(20 + 30 + 40, 2 + 3 + 4, 0, 0));
    assert_eq!(
        running_of(&rows[1])["total"],
        add(&pass_of(&rows[0])["total"], &pass["total"])
    );
}

// U9
/// The calibration and the `done` status come from the window: attempt 1's prompt of
/// the sequence, over the estimate of the internal view; the completion is the accepted
/// attempt's; the status keys are unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn window_calibrates() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_window").await;
    let mut rx = watch(&kernel, &a).await;
    let p = Metered::new(vec![
        Step::says(FOREIGN, rep(2000, 50, Some(500), None)),
        Step::says(DONE, rep(2600, 7, Some(1900), None)),
    ]);
    answers(&kernel, &a, &p).await;
    let done = done_status(&mut rx).await;
    assert_eq!(done["prompt_tokens"], json!(2000), "{done}");
    assert_eq!(done["completion_tokens"], json!(7), "{done}");
    assert!(done.get("prompt_tokens_estimated").is_none(), "{done}");
    for key in ["gen_seconds", "context_window"] {
        assert!(done.get(key).is_some(), "{key}: {done}");
    }
    let state = crate::state::state_for(&a);
    assert_eq!(*state.reported.lock().unwrap(), Some(2000));
    // the `default` type renders the internal view as it is: attempt 1's request is it
    let est = crate::context::estimate_tokens(&p.asked()[0]).max(1);
    let got = calib(&a).expect("calibrated");
    assert!(
        (got - 2000.0 / est as f64).abs() < 1e-9,
        "{got} vs 2000/{est}"
    );
}

// U10
/// The cost is priced from `used.pass.total`, the turn's attempts and the task pass
/// both; a model with no price entry is `{unpriced: true}` as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cost_from_total() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_cost").await;
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE,
                   "id": "u_cost_prov", "prices": {"test-model": {"input": 1.0, "output": 5.0}}}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": a.as_str(), "provider_id": "u_cost_prov"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let p = Metered::new(vec![
        Step::says("SUMMARY", rep(50, 4, None, None)),
        Step::says(FOREIGN, rep(100, 10, None, None)),
        Step::says(DONE, rep(200, 20, None, None)),
        Step::says(DONE, rep(10, 1, None, None)).named_model("unpriced-model"),
    ]);
    summarize(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    assert_eq!(pass_of(&row)["total"], c(350, 34, 0, 0), "{row}");
    let usd = row["attributes"]["cost"]["usd"].as_f64().expect("priced");
    assert!(
        (usd - (350.0 * 1.0 + 34.0 * 5.0) / 1e6).abs() < 1e-12,
        "{usd}"
    );
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    assert_eq!(
        row["attributes"]["cost"],
        json!({"unpriced": true}),
        "{row}"
    );
}

// U11
/// Cleanliness, `turn.rejected / (turn.accepted + turn.rejected)` over the four counts,
/// equals the hand-computed value for a retried turn (U2) and a stopped one (U3); the
/// task counts do not move it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanliness_ratio() {
    let ratio = |pass: &Value| {
        let r = total_of(&pass["turn"]["rejected"]) as f64;
        let ok = total_of(&pass["turn"]["accepted"]) as f64;
        r / (r + ok)
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_clean").await;
    let n = 1 + crate::constants::CALL_RETRIES;
    let mut steps = vec![
        // a task pass, then U2's turn
        Step::says("SUMMARY", rep(900, 90, None, None)),
        Step::says(FOREIGN, rep(100, 10, None, None)),
        Step::says(FOREIGN, rep(100, 10, None, None)),
        Step::says(DONE, rep(100, 20, None, None)),
    ];
    // U3's turn
    steps.extend((0..n).map(|_| Step::says(FOREIGN, rep(100, 10, None, None))));
    let p = Metered::new(steps);
    summarize(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    assert!(matches!(
        generate(&kernel, &a, &p).await,
        Ok(Generation::Stopped { .. })
    ));
    let rows = assistant_rows(&kernel, &a).await;
    // (110 + 110) / (110 + 110 + 120)
    assert!((ratio(pass_of(&rows[0])) - 220.0 / 340.0).abs() < 1e-12);
    assert_eq!(pass_of(&rows[0])["task"], c(900, 90, 0, 0));
    // nothing accepted: 1
    assert!((ratio(pass_of(&rows[1])) - 1.0).abs() < 1e-12);
    // over the whole run: (220 + 110 n) / (340 + 110 n)
    let n = n as f64;
    assert!(
        (ratio(running_of(&rows[1])) - (220.0 + 110.0 * n) / (340.0 + 110.0 * n)).abs() < 1e-12
    );
}

// U12
/// A task pass inside a turn, before its attempt 1 (the owed recall): its counts are in
/// `pass.task` and `total`; the window, the calibration and `turn` hold nothing of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_inside_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_task_in").await;
    let state = crate::state::state_for(&a);
    let p = Metered::new(vec![
        Step::says(DONE, rep(100, 10, None, None)),
        // the recall's compose, then attempt 1
        Step::says("{}", rep(300, 30, Some(100), None)),
        Step::says(DONE, rep(1000, 10, None, None)),
    ]);
    answers(&kernel, &a, &p).await;
    *state.recall_due.lock().unwrap() = true;
    answers(&kernel, &a, &p).await;
    assert_eq!(p.asked().len(), 3, "the recall asked the model");
    let row = last_row(&kernel, &a).await;
    let pass = pass_of(&row);
    assert_eq!(pass["task"], c(200, 30, 100, 0), "{row}");
    assert_eq!(pass["turn"]["accepted"], c(1000, 10, 0, 0));
    assert_eq!(pass["turn"]["rejected"], zero());
    assert_eq!(pass["total"], add(&pass["task"], &pass["turn"]["accepted"]));
    assert_eq!(row["attributes"]["window"], json!({"prompt": 1000}));
    assert_eq!(*state.reported.lock().unwrap(), Some(1000));
    let est = crate::context::estimate_tokens(&p.asked()[2]).max(1);
    let got = calib(&a).expect("calibrated");
    assert!(
        (got - 1000.0 / est as f64).abs() < 1e-9,
        "{got} vs 1000/{est}"
    );
}

// U14
/// An endpoint that reports no usage, on an accepted attempt, a failed one and a task
/// pass: all four counts 0, no window, the calibration unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_usage_report() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_nousage").await;
    *crate::state::state_for(&a).calib.lock().unwrap() = Some(2.5);
    let p = Metered::new(vec![
        Step::says("SUMMARY", no_usage()),
        Step::says(FOREIGN, no_usage()),
        Step::says(DONE, no_usage()),
    ]);
    summarize(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    let pass = pass_of(&row);
    for part in [
        &pass["turn"]["accepted"],
        &pass["turn"]["rejected"],
        &pass["task"],
        &pass["total"],
    ] {
        assert_eq!(*part, zero(), "{row}");
    }
    assert!(row["attributes"].get("window").is_none(), "{row}");
    assert_eq!(calib(&a), Some(2.5));
}

// U15
/// Cache counts larger than the prompt: `prompt` is 0, nothing panics, and `total` is
/// the sum of the parts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_counts_exceed_prompt() {
    let counts = crate::used::Counts::of(&rep(100, 5, Some(80), Some(50))).expect("reported");
    assert_eq!(counts.prompt, 0);
    assert_eq!(
        (counts.completion, counts.cache_read, counts.cache_write),
        (5, 80, 50)
    );
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_exceed").await;
    let p = Metered::new(vec![
        Step::says(FOREIGN, rep(100, 5, Some(80), Some(50))),
        Step::says(DONE, rep(10, 1, Some(i64::MAX), Some(i64::MAX))),
    ]);
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    let pass = pass_of(&row);
    assert_eq!(pass["turn"]["rejected"], c(0, 5, 80, 50), "{row}");
    assert_eq!(pass["turn"]["accepted"]["prompt"], json!(0));
    assert_eq!(
        row["attributes"]["window"],
        json!({"prompt": 0, "cache_read": 80, "cache_write": 50})
    );
    for k in ["prompt", "completion", "cache_read", "cache_write"] {
        let parts = pass["turn"]["accepted"][k]
            .as_i64()
            .unwrap()
            .saturating_add(pass["turn"]["rejected"][k].as_i64().unwrap())
            .saturating_add(pass["task"][k].as_i64().unwrap());
        assert_eq!(pass["total"][k].as_i64().unwrap(), parts, "{k}");
    }
}

// U16
/// A `default` pass leaves a large calibration; the next pass is `hermes`: the ratio is
/// cleared before its projection (the budget runs at 1.0). A model switch inside
/// `hermes` keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calib_cleared_on_type_change() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_calib").await;
    let state = crate::state::state_for(&a);
    // a hermes model's answer is its own form of `done` (#795: an answer has a call)
    let h_done = crate::model_format::Renderer::render(&crate::model_format::hermes::Hermes, DONE);
    let p = Metered::new(vec![
        Step::says(DONE, no_usage()),
        Step::says(&h_done, no_usage()).named_model(QWEN),
        Step::says(&h_done, no_usage()).named_model("qwen3-14b"),
    ]);
    answers(&kernel, &a, &p).await;
    assert_eq!(
        *state.last_type.lock().unwrap(),
        Some(crate::model_format::DEFAULT)
    );
    *state.calib.lock().unwrap() = Some(40.0);
    set_model(&kernel, &a, QWEN).await;
    answers(&kernel, &a, &p).await;
    let proj = state.projection.lock().unwrap().clone().expect("projected");
    assert_eq!(proj["calibration"], json!(1.0), "{proj}");
    assert_eq!(
        proj["fired"],
        json!(false),
        "the view is within the budget: {proj}"
    );
    assert_eq!(calib(&a), None);
    // the same type, another model: kept
    *state.calib.lock().unwrap() = Some(3.0);
    set_model(&kernel, &a, "qwen3-14b").await;
    answers(&kernel, &a, &p).await;
    let proj = state.projection.lock().unwrap().clone().expect("projected");
    assert_eq!(proj["calibration"], json!(3.0), "{proj}");
    assert_eq!(calib(&a), Some(3.0));
}

// U17
/// The process dies with counts pending: after the restart `running` goes on from the
/// newest row, the lost counts are in no row, and the sum holds over the rows after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_lost_on_crash() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_crash").await;
    let p = Metered::new(vec![
        Step::says(DONE, rep(100, 10, None, None)),
        // a thinking-only attempt, then the link fails: pending
        Step::says("<think>x</think>", rep(70, 7, None, None)),
        Step::link_fails(),
        Step::says(DONE, rep(200, 20, None, None)),
        Step::says(DONE, rep(300, 30, None, None)),
    ]);
    answers(&kernel, &a, &p).await;
    assert!(generate(&kernel, &a, &p).await.is_err(), "the link failed");
    assert_ne!(
        *crate::state::state_for(&a).used_pending.lock().unwrap(),
        crate::used::Pending::default()
    );
    crate::state::drop_state(&a);
    answers(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    let rows = assistant_rows(&kernel, &a).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(pass_of(&rows[1])["turn"]["rejected"], zero(), "lost");
    for i in 1..3 {
        assert_eq!(
            running_of(&rows[i])["total"],
            add(
                &running_of(&rows[i - 1])["total"],
                &pass_of(&rows[i])["total"]
            )
        );
    }
    assert_eq!(running_of(&rows[2])["total"], c(600, 60, 0, 0));
}

// U18
/// A harness-stopped turn, then the next send: the stopped row carries `used` with
/// nothing accepted, and the next row's `running` includes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_turn_counts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_stopped").await;
    let n = 1 + crate::constants::CALL_RETRIES;
    let mut steps: Vec<Step> = (0..n)
        .map(|_| Step::says(FOREIGN, rep(100, 10, None, None)))
        .collect();
    steps.push(Step::says(DONE, rep(50, 5, None, None)));
    let p = Metered::new(steps);
    assert!(matches!(
        generate(&kernel, &a, &p).await,
        Ok(Generation::Stopped { .. })
    ));
    answers(&kernel, &a, &p).await;
    let rows = assistant_rows(&kernel, &a).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(pass_of(&rows[0])["turn"]["accepted"], zero());
    let n = n as i64;
    assert_eq!(
        pass_of(&rows[0])["turn"]["rejected"],
        c(100 * n, 10 * n, 0, 0)
    );
    assert_eq!(
        running_of(&rows[1])["total"],
        add(&pass_of(&rows[0])["total"], &pass_of(&rows[1])["total"])
    );
    assert_eq!(
        running_of(&rows[1])["turn"]["rejected"],
        c(100 * n, 10 * n, 0, 0)
    );
}

// U19
/// A type restart inside a turn: the discarded attempt is `turn.rejected`, and the
/// window is the restarted sequence's attempt 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_counts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_retype").await;
    set_model(&kernel, &a, QWEN).await;
    // a hermes pass; the reply names a `default` model and its call is not hermes
    let p = Metered::new(vec![
        Step::says(DONE, rep(100, 10, None, None)).named_model(PLAIN),
        Step::says(DONE, rep(200, 20, None, None)).named_model(PLAIN),
    ]);
    answers(&kernel, &a, &p).await;
    assert_eq!(p.asked().len(), 2);
    assert_eq!(
        p.asked()[1].len(),
        p.asked()[0].len(),
        "a restart, not a re-ask"
    );
    let row = last_row(&kernel, &a).await;
    let pass = pass_of(&row);
    assert_eq!(pass["turn"]["rejected"], c(100, 10, 0, 0), "{row}");
    assert_eq!(pass["turn"]["accepted"], c(200, 20, 0, 0));
    assert_eq!(row["attributes"]["window"], json!({"prompt": 200}));
}

// U20
/// A task pass outside a turn: `compose` of the `context_recall` verb, and the fold
/// summarizer on the chat's pass. Its counts wait, then land in `pass.task` of the next
/// stored row; the running sum holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_outside_turn() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_task_out").await;
    let p = Metered::new(vec![
        Step::says(DONE, rep(100, 10, None, None)),
        Step::says("SUMMARY", rep(40, 4, None, None)),
        Step::says(DONE, rep(200, 20, None, None)),
    ]);
    answers(&kernel, &a, &p).await;
    // the verb's pass runs on the bundle's provider (the scripted one)
    let s = script(&a, vec![pass("{}")], Duration::ZERO);
    *s.report.lock().unwrap() = rep(300, 30, None, None);
    let r = kernel
        .operator_send(vec![Item::new(
            a.clone(),
            json!({"type": "context_recall"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(s.passes_served(), 1, "the recall asked the model");
    assert_eq!(summarize(&kernel, &a, &p).await, "SUMMARY");
    let pending = *crate::state::state_for(&a).used_pending.lock().unwrap();
    assert_eq!(
        (pending.task.prompt, pending.task.completion),
        (340, 34),
        "both wait"
    );
    answers(&kernel, &a, &p).await;
    let rows = assistant_rows(&kernel, &a).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(pass_of(&rows[1])["task"], c(340, 34, 0, 0), "{}", rows[1]);
    assert_eq!(pass_of(&rows[1])["turn"]["accepted"], c(200, 20, 0, 0));
    assert_eq!(
        running_of(&rows[1])["total"],
        add(&pass_of(&rows[0])["total"], &pass_of(&rows[1])["total"])
    );
    assert_eq!(
        *crate::state::state_for(&a).used_pending.lock().unwrap(),
        crate::used::Pending::default()
    );
}

// U21
/// A task pass that fails on the link or is cancelled adds nothing, and the turn's
/// attempt after it reads only its own report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_task_adds_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_task_fail").await;
    let p = Metered::new(vec![
        Step::says(DONE, rep(100, 77, None, None)),
        Step::link_fails(),
        Step::hangs(),
        Step::says(DONE, rep(200, 20, None, None)),
    ]);
    answers(&kernel, &a, &p).await;
    assert_eq!(summarize(&kernel, &a, &p).await, "", "the link failed");
    let cancelled =
        tokio::time::timeout(Duration::from_millis(300), summarize(&kernel, &a, &p)).await;
    assert!(cancelled.is_err(), "cancelled");
    assert_eq!(
        *crate::state::state_for(&a).used_pending.lock().unwrap(),
        crate::used::Pending::default()
    );
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    assert_eq!(pass_of(&row)["task"], zero(), "{row}");
    assert_eq!(pass_of(&row)["turn"]["accepted"], c(200, 20, 0, 0));
    assert_eq!(pass_of(&row)["total"], c(200, 20, 0, 0));
}

// U22
/// A `stop` the model writes is a well-formed call the chat took: `turn.accepted`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_stop_is_accepted() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_model_stop").await;
    let mut args = Map::new();
    args.insert("reason".to_string(), json!("the file does not exist"));
    let stop = crate::tool_parse::render_call("stop", &args);
    let p = Metered::new(vec![Step::says(&stop, rep(100, 10, None, None))]);
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    assert_eq!(pass_of(&row)["turn"]["accepted"], c(100, 10, 0, 0), "{row}");
    assert_eq!(pass_of(&row)["turn"]["rejected"], zero());
}

// U23
/// The shape: `pass` and `running` have exactly `turn.accepted`, `turn.rejected`,
/// `task` and `total`, each exactly the four counts; `total` is the sum of the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn used_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "u_shape").await;
    let p = Metered::new(vec![
        Step::says("SUMMARY", rep(30, 3, Some(1), None)),
        Step::says(FOREIGN, rep(100, 10, Some(2), Some(3))),
        Step::says(DONE, rep(200, 20, Some(4), Some(5))),
    ]);
    summarize(&kernel, &a, &p).await;
    answers(&kernel, &a, &p).await;
    let row = last_row(&kernel, &a).await;
    let keys = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_object().expect("an object").keys().cloned().collect();
        k.sort();
        k
    };
    let used = &row["attributes"]["used"];
    assert_eq!(keys(used), ["pass", "running"]);
    let four = ["cache_read", "cache_write", "completion", "prompt"];
    for group in [&used["pass"], &used["running"]] {
        assert_eq!(keys(group), ["task", "total", "turn"]);
        assert_eq!(keys(&group["turn"]), ["accepted", "rejected"]);
        for part in [
            &group["turn"]["accepted"],
            &group["turn"]["rejected"],
            &group["task"],
            &group["total"],
        ] {
            assert_eq!(keys(part), four);
            assert!(four.iter().all(|k| part[*k].is_i64()), "{part}");
        }
        let sum = add(
            &add(&group["turn"]["accepted"], &group["turn"]["rejected"]),
            &group["task"],
        );
        assert_eq!(group["total"], sum);
    }
    assert_eq!(used["pass"]["task"], c(29, 3, 1, 0));
}
