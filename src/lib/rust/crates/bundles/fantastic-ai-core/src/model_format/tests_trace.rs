//! The traces (#766, W9): E1-E20 and M19 of spec 2.9 and 2.1.
//!
//! Every turn here is the real pass path (`run_generation`) over a scripted provider
//! that reports per request, as the remote provider does. A restart is the chat's state
//! dropped (`drop_state`) and its head forgotten; on `sqlite_store` the file is closed
//! too, so the rows come back from disk. A small cap is a test's own
//! ([`trace::set_cap_for_test`]); the shipped one is [`crate::constants::TRACE_CAP`].
//!
//! The readers of the store that apply no role filter, found for E3 (W9's first step):
//! the head cache's load and refresh (`memory::head`, `memory::refresh_head`), which
//! feed `rows`, `context_clear`'s members, `context_recall`, `load_view`, the card rows
//! and the pause; the whole-store reads of `fork` (`fork::whole_store`) and of
//! `context_forget` (`retrieval.rs`, its view rows and the rows behind the last clear);
//! and the store's `search` under `grep`. `recall`, `history` and the fold's members
//! read `load_history`, which keeps the rows with a chat `role`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Reply};
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Handler, Item, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use tempfile::TempDir;

use super::pass::Pass;
use super::tests_cache::{cache, conv, open, refs, rows_n, run, Count, Opts};
use super::trace;
use crate::agent_loop::{run_generation, BackendConfig, GenFailure, Generation};
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderReport, ProviderStream};
use crate::tests::{base_kernel, mk_kernel};

pub(super) const CFG: BackendConfig = BackendConfig {
    name: "mock_backend",
    model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
};
/// A foreign call tag: the `default` reader fails the attempt at it.
pub(super) const FOREIGN: &str = "Sending.\n<invoke name=\"send\">";
/// The internal form, foreign to a `hermes` model.
pub(super) const DONE: &str = r#"<call name="done"></call>"#;
/// An answer with prose and a call (#795: an answer with no call is re-asked).
const ANSWER: &str = r#"the answer <call name="done"></call>"#;
/// A model id of the `default` type.
pub(super) const PLAIN: &str = "test-model";
/// A model id of the `hermes` type.
pub(super) const QWEN: &str = "qwen3-8b";
/// Another model id of the `hermes` type.
const QWEN_14: &str = "qwen3-14b";

// ─── the scripted endpoint ───────────────────────────────────────────────────

/// One scripted request.
pub(super) struct Step {
    text: String,
    /// The model the endpoint names from this request on; empty keeps the last one.
    model: &'static str,
    /// The reply's counts, written when the stream ended whole; `None` writes nothing.
    report: Option<ProviderReport>,
    /// After its text the stream stays open: only a drop ends it.
    hang: bool,
    /// The request itself never returns: only a drop ends it.
    stall: bool,
    /// Its text is sent as written, never in the request's type's form.
    raw: bool,
}

impl Step {
    pub(super) fn says(text: &str) -> Self {
        Self {
            text: text.to_string(),
            model: "",
            report: Some(ProviderReport::default()),
            hang: false,
            stall: false,
            raw: false,
        }
    }

    /// `text` as written, whatever type the request is: a call form foreign to it.
    pub(super) fn raw(text: &str) -> Self {
        Self {
            raw: true,
            ..Self::says(text)
        }
    }

    pub(super) fn reports(mut self, report: ProviderReport) -> Self {
        self.report = Some(report);
        self
    }

    pub(super) fn unreported(mut self) -> Self {
        self.report = None;
        self
    }

    pub(super) fn named_model(mut self, model: &'static str) -> Self {
        self.model = model;
        self
    }

    pub(super) fn hangs(text: &str) -> Self {
        Self {
            hang: true,
            ..Self::says(text).unreported()
        }
    }

    pub(super) fn stalls() -> Self {
        Self {
            stall: true,
            ..Self::says("").unreported()
        }
    }
}

/// An endpoint that reports per request: the prompt and cache counts are cleared before
/// each request, the reply's counts written when its stream ended whole.
pub(super) struct Scripted {
    steps: Mutex<VecDeque<Step>>,
    asked: Mutex<Vec<Vec<Value>>>,
    model: Mutex<String>,
    report: Arc<Mutex<ProviderReport>>,
}

impl Scripted {
    pub(super) fn new(model: &str, steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            asked: Mutex::new(Vec::new()),
            model: Mutex::new(model.to_string()),
            report: Arc::new(Mutex::new(ProviderReport::default())),
        })
    }

    pub(super) fn push(&self, steps: Vec<Step>) {
        self.steps.lock().unwrap().extend(steps);
    }

    pub(super) fn set_model(&self, model: &str) {
        *self.model.lock().unwrap() = model.to_string();
    }

    pub(super) fn asked(&self) -> Vec<Vec<Value>> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for Scripted {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.asked.lock().unwrap().push(messages.to_vec());
        let mut step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Step::says(DONE));
        // A STEP IS INTERNAL TEXT, written in the form of the type its request was made
        // under (the model before this step names another): `DONE` is a hermes chat's
        // own `done` (#795: an answer has a call). Text of no call form is unchanged.
        if !step.raw {
            let kind = super::rules::type_of(super::rules::shipped(), &self.model.lock().unwrap());
            step.text = super::of(kind).calls.render(&step.text);
        }
        if !step.model.is_empty() {
            *self.model.lock().unwrap() = step.model.to_string();
        }
        if step.stall {
            futures_util::future::pending::<()>().await;
        }
        let mut evs: Vec<Result<ProviderEvent, ProviderError>> = Vec::new();
        if !step.text.is_empty() {
            evs.push(Ok(ProviderEvent::Token(step.text.clone())));
        }
        let head = futures_util::stream::iter(evs);
        if step.hang {
            return Ok(Box::pin(head.chain(futures_util::stream::pending())));
        }
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

/// The endpoint's reply counts.
pub(super) fn rep(
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

// ─── a store that refuses ────────────────────────────────────────────────────

/// A `ram_store` that refuses one thing: every `append` of trace rows only
/// (`"append"`), or every `prune` (`"prune"`).
struct Refusing {
    inner: fantastic_ram_store::RamStoreBundle,
    refuse: &'static str,
}

#[async_trait]
impl Handler for Refusing {
    type Call = fantastic_ram_store::verbs::Call;
    fn name(&self) -> &str {
        "refusing_store"
    }
    fn man(&self) -> &'static str {
        self.inner.man()
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        self.inner.verbs()
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        self.inner.dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        self.inner.record()
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        use fantastic_ram_store::verbs::Call;
        let refused = match &call {
            Call::Append(a) => {
                self.refuse == "append"
                    && a.rows
                        .iter()
                        .all(|r| r.rest.get("kind") == Some(&json!(trace::KIND)))
            }
            Call::Prune(_) => self.refuse == "prune",
            _ => false,
        };
        if refused {
            return Ok(Some(fantastic_kernel::refusal::refusal(
                fantastic_kernel::refusal::BAD_ARG,
                format!("{}: refused by the test store", self.refuse),
            )));
        }
        self.inner.handle(agent_id, call, kernel).await
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// One kernel call as the operator; its result.
pub(super) async fn operator(kernel: &Arc<Kernel>, to: &str, payload: Value) -> Value {
    kernel
        .operator_send(vec![Item::new(AgentId::from(to), payload)])
        .await
        .remove(0)
        .result
}

/// One verb on the chat; its result.
pub(super) async fn verb(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Value {
    operator(kernel, agent.as_str(), payload).await
}

/// A chat `id` on the mock bundle keeping its rows in `store`, with `model` on its
/// record when given.
pub(super) async fn chat_on(
    kernel: &Arc<Kernel>,
    id: &str,
    store: &str,
    model: Option<&str>,
) -> AgentId {
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": "mock_backend.tools",
        "id": id,
        "context_window": 32768,
        "memory_store_id": store,
        "can": {store: {"call": ["*"]}, "cli": {"call": ["*"]}},
    });
    if let Some(m) = model {
        create["model"] = json!(m);
    }
    let r = operator(kernel, "kernel_state", create).await;
    assert!(r.get("error").is_none(), "create {id}: {r}");
    AgentId::from(id)
}

/// A store agent `id` of `module` under the root, with `extra` on its record.
pub(super) async fn store_agent(kernel: &Arc<Kernel>, id: &str, module: &str, extra: Value) {
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": module,
        "id": id,
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        create[k] = v;
    }
    let r = operator(kernel, "kernel_state", create).await;
    assert!(r.get("error").is_none(), "create {id}: {r}");
}

/// Set a chat's record `model`.
pub(super) async fn set_model(kernel: &Arc<Kernel>, agent: &AgentId, model: &str) {
    let r = operator(
        kernel,
        "kernel_state",
        json!({"type": "update_agent", "id": agent.as_str(), "model": model}),
    )
    .await;
    assert!(r.get("error").is_none(), "update_agent refused: {r}");
}

/// A chat of the `hermes` type on the shared test store.
async fn mk_hermes(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let (kernel, a) = mk_kernel(tmp, tag).await;
    set_model(&kernel, &a, QWEN).await;
    (kernel, a)
}

/// One generation of `agent` as `client`, run as the agent, as `send` runs it.
pub(super) async fn generate(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
    provider: &Arc<Scripted>,
    client: &str,
) -> Result<Generation, GenFailure> {
    let p: Arc<dyn Provider> = Arc::clone(provider) as Arc<dyn Provider>;
    let state = crate::state::state_for(agent);
    as_agent(
        agent.clone(),
        run_generation(&p, agent, &state, "go", kernel, client, CFG),
    )
    .await
}

/// A generation that answered: its text and assistant row id.
pub(super) async fn answered(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
    provider: &Arc<Scripted>,
    client: &str,
) -> (String, i64) {
    match generate(kernel, agent, provider, client).await {
        Ok(Generation::Answered {
            text, message_id, ..
        }) => (text, message_id),
        Ok(Generation::Stopped { reason, .. }) => panic!("{client}: stopped: {reason}"),
        Err(_) => panic!("{client}: the generation failed"),
    }
}

/// The same generation, spawned: its handle, to drop it mid-pass.
fn spawn_generate(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
    provider: &Arc<Scripted>,
    client: &str,
) -> tokio::task::JoinHandle<()> {
    let (k, a, p, c) = (
        Arc::clone(kernel),
        agent.clone(),
        Arc::clone(provider),
        client.to_string(),
    );
    tokio::spawn(async move {
        let _ = generate(&k, &a, &p, &c).await;
    })
}

/// Wait (at most 5 s) until `provider` was asked `n` times.
pub(super) async fn until_asked(provider: &Scripted, n: usize) {
    for _ in 0..200 {
        if provider.asked.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the endpoint was never asked {n} times");
}

/// Every row the chat's store holds, read as the chat, in id order.
pub(super) async fn store(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res["rows"].as_array().cloned().unwrap_or_default()
}

/// The chat's trace rows, in id order.
pub(super) async fn traces(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    store(kernel, agent)
        .await
        .into_iter()
        .filter(|r| r["kind"] == json!(trace::KIND))
        .collect()
}

/// The trace rows of one send.
pub(super) async fn traces_of(kernel: &Arc<Kernel>, agent: &AgentId, client: &str) -> Vec<Value> {
    traces(kernel, agent)
        .await
        .into_iter()
        .filter(|r| r["client_id"] == json!(client))
        .collect()
}

/// The event names of `rows`, in order.
pub(super) fn names(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["event"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The ids of `rows`, in order.
pub(super) fn ids(rows: &[Value]) -> Vec<i64> {
    rows.iter().filter_map(|r| r["id"].as_i64()).collect()
}

/// The chat's trace count in RAM.
fn ram_count(agent: &AgentId) -> Option<usize> {
    *crate::state::state_for(agent).trace_count.lock().unwrap()
}

/// Every trace row of the chat pruned from its store, by id.
async fn prune_traces(kernel: &Arc<Kernel>, agent: &AgentId) {
    let doomed = ids(&traces(kernel, agent).await);
    if doomed.is_empty() {
        return;
    }
    as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "prune", "ids": doomed})),
    )
    .await
    .expect("prune");
}

/// A restart of the chat: its state dropped, its head forgotten.
pub(super) fn restart(kernel: &Kernel, agent: &AgentId) {
    crate::state::drop_state(agent);
    crate::memory::forget_head(agent, kernel);
}

/// `n` events written as one burst of `client`, through the chat's writer.
async fn burst(kernel: &Arc<Kernel>, agent: &AgentId, client: &str, n: usize) {
    let mut ev = trace::Events::default();
    for i in 0..n {
        let mut fields = Map::new();
        fields.insert("rows".into(), json!(i));
        fields.insert("kept".into(), json!(0));
        fields.insert("kept_endpoint".into(), json!(0));
        ev.push("turn", super::DEFAULT, "render_warm", fields);
    }
    let state = crate::state::state_for(agent);
    as_agent(
        agent.clone(),
        trace::written(agent, kernel, &state, client, None, ev.into_rows()),
    )
    .await;
}

/// The next turn's messages, as the assembly builds them.
pub(super) async fn assembled(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let state = crate::state::state_for(agent);
    as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            agent,
            &state,
            kernel,
            "c",
            crate::caching::Caching::ProviderCache,
        ),
    )
    .await
    .expect("assemble")
    .0
}

/// The leading messages the chain of request `now` keeps from request `before`: equal
/// role and content. The arriving message of `before` is a stored row now (#795), shown
/// as the same bytes in `now`, so it is kept with the rest: its link is in the chain
/// though it is never cached as an entry (`cache.rs` `arriving`).
fn kept_between(before: &[Value], now: &[Value]) -> usize {
    before
        .iter()
        .zip(now)
        .take_while(|(a, b)| a["role"] == b["role"] && a["content"] == b["content"])
        .count()
}

/// The `stop` call with `reason`, in the internal form.
pub(super) fn stop_call(reason: &str) -> String {
    let mut args = Map::new();
    args.insert("reason".to_string(), json!(reason));
    crate::tool_parse::render_call("stop", &args)
}

// ─── E1-E20 ──────────────────────────────────────────────────────────────────

// E1
/// Every event carries `mode`, `client_id`, `type`, and `row` when the turn stored one;
/// no `role`, an empty `content`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_event_has_ids() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e1").await;
    let p = Scripted::new(
        PLAIN,
        vec![
            Step::says(FOREIGN).reports(rep(1000, 30, Some(200), None)),
            Step::says(DONE).reports(rep(1100, 5, Some(900), None)),
            Step::says("<think>only thinking</think>"),
            Step::says(DONE),
        ],
    );
    let (_, mid) = answered(&kernel, &a, &p, "e1_a").await;
    let rows = traces_of(&kernel, &a, "e1_a").await;
    assert_eq!(
        names(&rows),
        ["prefix_cache", "retry", "prefix_cache", "render_cold"],
        "{rows:?}"
    );
    for r in &rows {
        assert_eq!(r["kind"], json!("trace"), "{r}");
        assert_eq!(r["mode"], json!("turn"), "{r}");
        assert_eq!(r["type"], json!("default"), "{r}");
        assert_eq!(r["row"], json!(mid), "{r}");
        assert_eq!(r["content"], json!(""), "{r}");
        assert!(r.get("role").is_none(), "{r}");
    }
    // a thinking-only attempt is re-asked inside its turn (#795): a `retry`, and every
    // row names the row the turn stored
    let (_, mid) = answered(&kernel, &a, &p, "e1_think").await;
    let think = traces_of(&kernel, &a, "e1_think").await;
    assert!(names(&think).contains(&"retry".to_string()), "{think:?}");
    for r in &think {
        assert_eq!(r["row"], json!(mid), "{r}");
        assert_eq!(r["mode"], json!("turn"), "{r}");
    }
}

// E2
/// No trace row reaches a model view, D or H, across a switch: the next view's bytes
/// and its `kept` are the same with the trace rows and without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_never_in_view() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e2").await;
    let p = Scripted::new(PLAIN, vec![Step::says(DONE)]);
    answered(&kernel, &a, &p, "e2_a").await;
    set_model(&kernel, &a, QWEN).await;
    p.set_model(QWEN);
    answered(&kernel, &a, &p, "e2_b").await;
    answered(&kernel, &a, &p, "e2_c").await;
    let all = traces(&kernel, &a).await;
    assert!(
        names(&all).contains(&"model_switched".to_string()),
        "{all:?}"
    );
    for req in p.asked() {
        let text = Value::Array(req).to_string();
        for event in [
            "render_cold",
            "render_warm",
            "model_switched",
            "prefix_cache",
        ] {
            assert!(!text.contains(event), "{event} in a request: {text}");
        }
    }
    let state = crate::state::state_for(&a);
    let meta = crate::helpers::agent_meta(&a, &kernel);
    let kept_of = |msgs: &[Value]| {
        let pass = Pass::typed_for_test(
            Arc::clone(&p) as Arc<dyn Provider>,
            Arc::clone(&state),
            super::hermes::NAME,
            Some(QWEN),
        );
        let view = pass.view(msgs, &meta);
        (view.messages.clone(), view.kept)
    };
    let with = assembled(&kernel, &a).await;
    let (typed_with, kept_with) = kept_of(&with);
    prune_traces(&kernel, &a).await;
    crate::memory::forget_head(&a, &kernel);
    let without = assembled(&kernel, &a).await;
    let (typed_without, kept_without) = kept_of(&without);
    assert_eq!(with, without, "the view with and without trace rows");
    assert_eq!(typed_with, typed_without);
    assert_eq!(kept_with, kept_without);
}

// E3
/// Trace rows are in no model view, no `recall`, no `grep`, no fold's members, no clear's
/// members, no fork's copy, no head cache, no `rows`; a store `search` for `null` and for
/// `trace` finds none; `context_forget` leaves them and their count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_hidden_from_readers() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e3").await;
    let p = Scripted::new(PLAIN, vec![]);
    answered(&kernel, &a, &p, "e3_a").await;
    let (_, last) = answered(&kernel, &a, &p, "e3_b").await;
    let t = ids(&traces(&kernel, &a).await);
    assert_eq!(t.len(), 2, "one render row per turn");
    let is_trace_id = |v: &Value| v.as_i64().is_some_and(|i| t.contains(&i));

    // the head cache, loaded and refreshed, and the `rows` verb over it
    crate::memory::forget_head(&a, &kernel);
    for r in as_agent(a.clone(), crate::memory::head(&a, &kernel))
        .await
        .expect("head")
    {
        assert_ne!(r["kind"], json!("trace"), "head: {r}");
    }
    for r in as_agent(a.clone(), crate::memory::refresh_head(&a, &kernel))
        .await
        .expect("refresh")
    {
        assert_ne!(r["kind"], json!("trace"), "refresh: {r}");
    }
    let rows = verb(&kernel, &a, json!({"type": "rows"})).await;
    for r in rows["rows"].as_array().cloned().unwrap_or_default() {
        assert_ne!(r["kind"], json!("trace"), "rows: {r}");
    }
    // the model view
    let view = Value::Array(assembled(&kernel, &a).await).to_string();
    assert!(
        !view.contains("render_cold") && !view.contains("render_warm"),
        "{view}"
    );
    // recall
    for ask in [
        json!({"type": "recall"}),
        json!({"type": "recall", "query": "render"}),
    ] {
        let out = verb(&kernel, &a, ask.clone()).await.to_string();
        assert!(
            !out.contains("render_cold") && !out.contains("render_warm"),
            "{ask}: {out}"
        );
    }
    // grep, and the store's own search
    for q in ["render", "trace", "null"] {
        let out = verb(&kernel, &a, json!({"type": "grep", "query": q})).await;
        for m in out["matches"].as_array().cloned().unwrap_or_default() {
            assert!(!is_trace_id(&m["id"]), "grep {q}: {m}");
        }
    }
    for q in ["null", "trace"] {
        let out = as_agent(
            a.clone(),
            crate::memory::store_call(&a, &kernel, json!({"type": "search", "query": q})),
        )
        .await
        .expect("search");
        for m in out["matches"].as_array().cloned().unwrap_or_default() {
            assert!(!is_trace_id(&m["id"]), "search {q}: {m}");
        }
    }
    // a fork's copy
    let count = ram_count(&a);
    let forked = verb(&kernel, &a, json!({"type": "fork", "at": last})).await;
    let child = AgentId::from(forked["id"].as_str().expect("the fork's id"));
    assert!(
        traces(&kernel, &child).await.is_empty(),
        "the fork copied trace rows"
    );
    assert_eq!(ram_count(&a), count);
    // a fold's and a clear's members
    let fold = verb(
        &kernel,
        &a,
        json!({"type": "context_fold", "summary": "the span"}),
    )
    .await;
    assert!(fold.get("error").is_none(), "{fold}");
    let clear = verb(&kernel, &a, json!({"type": "context_clear"})).await;
    assert!(clear.get("error").is_none(), "{clear}");
    for r in store(&kernel, &a).await {
        for m in r["refs"].as_array().cloned().unwrap_or_default() {
            assert!(!is_trace_id(&m), "{} holds a trace row: {r}", r["kind"]);
        }
    }
    // context_forget, behind the clear too
    let forget = verb(
        &kernel,
        &a,
        json!({"type": "context_forget", "include_cleared": true}),
    )
    .await;
    assert!(forget.get("error").is_none(), "{forget}");
    assert_eq!(ids(&traces(&kernel, &a).await), t);
    assert_eq!(ram_count(&a), Some(t.len()));
}

// E4
/// A burst of three times the cap: after every write the count is at most the cap, and
/// the survivors are the newest rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_holds_under_burst() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e4").await;
    let cap = 5;
    trace::set_cap_for_test(&a, cap);
    let p = Scripted::new(PLAIN, vec![]);
    let mut seen: Vec<i64> = Vec::new();
    for round in 0..3 {
        answered(&kernel, &a, &p, &format!("e4_turn{round}")).await;
        seen.extend(ids(&traces(&kernel, &a).await));
        burst(&kernel, &a, &format!("e4_burst{round}"), cap).await;
        let rows = traces(&kernel, &a).await;
        seen.extend(ids(&rows));
        assert!(rows.len() <= cap, "round {round}: {}", rows.len());
        assert_eq!(ram_count(&a), Some(rows.len()), "round {round}");
    }
    // one write over the cap on its own
    burst(&kernel, &a, "e4_big", cap + 2).await;
    let rows = traces(&kernel, &a).await;
    seen.extend(ids(&rows));
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(rows.len(), cap);
    assert_eq!(
        ids(&rows),
        seen[seen.len() - cap..].to_vec(),
        "the newest survive"
    );
    assert!(rows.iter().all(|r| r["client_id"] == json!("e4_big")));
}

// E5
/// The prune touches no turn and no `view`, `keep` or `mark` row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_spares_other_rows() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e5").await;
    trace::set_cap_for_test(&a, 2);
    let p = Scripted::new(PLAIN, vec![]);
    answered(&kernel, &a, &p, "e5_a").await;
    let (_, last) = answered(&kernel, &a, &p, "e5_b").await;
    as_agent(
        a.clone(),
        crate::memory::save_history(
            &a,
            &kernel,
            &[
                json!({"kind": "view", "ids": [last]}),
                json!({"kind": "keep", "of": last, "summary": "kept"}),
                json!({"kind": "mark", "label": "m", "at": last}),
            ],
        ),
    )
    .await
    .expect("rows");
    let others: Vec<Value> = store(&kernel, &a)
        .await
        .into_iter()
        .filter(|r| r["kind"] != json!(trace::KIND))
        .collect();
    assert!(others.len() >= 7, "{others:?}");
    for round in 0..3 {
        burst(&kernel, &a, &format!("e5_burst{round}"), 3).await;
    }
    let after: Vec<Value> = store(&kernel, &a)
        .await
        .into_iter()
        .filter(|r| r["kind"] != json!(trace::KIND))
        .collect();
    assert_eq!(after, others);
    assert_eq!(traces(&kernel, &a).await.len(), 2);
}

// E6
/// On `sqlite_store`: at most the cap of trace rows comes back after a reboot; the count
/// is read once per process (a row planted behind the writer's back is not counted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traces_survive_restart() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register(
            fantastic_sqlite_store::HANDLER_MODULE,
            fantastic_sqlite_store::SqliteStoreBundle,
        );
    })
    .await;
    store_agent(
        &kernel,
        "tr_e6_db",
        fantastic_sqlite_store::HANDLER_MODULE,
        json!({"path": "tr_e6.db"}),
    )
    .await;
    let a = chat_on(&kernel, "tr_e6_chat", "tr_e6_db", Some(PLAIN)).await;
    let cap = 4;
    trace::set_cap_for_test(&a, cap);
    let p = Scripted::new(PLAIN, vec![]);
    for i in 0..6 {
        answered(&kernel, &a, &p, &format!("e6_{i}")).await;
    }
    let before = ids(&traces(&kernel, &a).await);
    assert_eq!(before.len(), cap);
    // the reboot: the state and the head gone, the file closed and opened again
    restart(&kernel, &a);
    assert!(fantastic_sqlite_store::close_all(&kernel).is_empty());
    assert_eq!(ram_count(&a), None);
    assert_eq!(
        ids(&traces(&kernel, &a).await),
        before,
        "back from the file"
    );
    answered(&kernel, &a, &p, "e6_after").await;
    assert_eq!(ram_count(&a), Some(cap), "read once, then kept");
    assert_eq!(traces(&kernel, &a).await.len(), cap);
    // a row the writer did not write is not in its count: it is not read again
    as_agent(
        a.clone(),
        crate::memory::store_call(
            &a,
            &kernel,
            json!({"type": "append", "rows": [
                {"kind": "trace", "event": "planted", "mode": "turn", "type": "default", "content": ""}
            ]}),
        ),
    )
    .await
    .expect("plant");
    answered(&kernel, &a, &p, "e6_next").await;
    assert_eq!(ram_count(&a), Some(cap));
    assert_eq!(traces(&kernel, &a).await.len(), cap + 1);
}

// E7
/// A refused trace append, and a refused prune: the turn succeeds, its reply unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_trace_never_fails_turn() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register(
            "refuse_append.tools",
            Refusing {
                inner: fantastic_ram_store::RamStoreBundle::default(),
                refuse: "append",
            },
        );
        k.bundles.register(
            "refuse_prune.tools",
            Refusing {
                inner: fantastic_ram_store::RamStoreBundle::default(),
                refuse: "prune",
            },
        );
    })
    .await;
    store_agent(&kernel, "tr_e7_na", "refuse_append.tools", json!({})).await;
    store_agent(&kernel, "tr_e7_np", "refuse_prune.tools", json!({})).await;

    let a = chat_on(&kernel, "tr_e7_append", "tr_e7_na", Some(PLAIN)).await;
    let p = Scripted::new(PLAIN, vec![Step::says(FOREIGN), Step::says(ANSWER)]);
    let (text, mid) = answered(&kernel, &a, &p, "e7_a").await;
    assert_eq!(text, ANSWER);
    let rows = store(&kernel, &a).await;
    let row = rows
        .iter()
        .find(|r| r["id"] == json!(mid))
        .expect("the assistant row");
    assert_eq!(row["content"], json!(ANSWER));
    assert!(traces(&kernel, &a).await.is_empty());

    let b = chat_on(&kernel, "tr_e7_prune", "tr_e7_np", Some(PLAIN)).await;
    trace::set_cap_for_test(&b, 1);
    let p = Scripted::new(PLAIN, vec![]);
    for i in 0..3 {
        let (text, _) = answered(&kernel, &b, &p, &format!("e7_p{i}")).await;
        assert_eq!(text, DONE);
    }
    assert_eq!(traces(&kernel, &b).await.len(), 3, "nothing pruned");
    assert_eq!(ram_count(&b), Some(3));
}

// E8
/// `prefix_cache` is written only when the endpoint reported cache counts, with the
/// counts it reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefix_cache_only_when_reported() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e8").await;
    let p = Scripted::new(
        PLAIN,
        vec![
            Step::says(DONE).reports(rep(100, 5, Some(40), Some(10))),
            Step::says(DONE).reports(rep(100, 5, None, None)),
            Step::says(DONE).reports(rep(100, 5, Some(70), None)),
        ],
    );
    answered(&kernel, &a, &p, "e8_both").await;
    answered(&kernel, &a, &p, "e8_none").await;
    answered(&kernel, &a, &p, "e8_read").await;
    let both: Vec<Value> = traces_of(&kernel, &a, "e8_both")
        .await
        .into_iter()
        .filter(|r| r["event"] == json!("prefix_cache"))
        .collect();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0]["cache_read"], json!(40));
    assert_eq!(both[0]["cache_write"], json!(10));
    let none = traces_of(&kernel, &a, "e8_none").await;
    assert!(
        !names(&none).contains(&"prefix_cache".to_string()),
        "{none:?}"
    );
    let read: Vec<Value> = traces_of(&kernel, &a, "e8_read")
        .await
        .into_iter()
        .filter(|r| r["event"] == json!("prefix_cache"))
        .collect();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0]["cache_read"], json!(70));
    assert!(read[0].get("cache_write").is_none(), "{}", read[0]);
}

// E9
/// Cap 0: nothing is written, and the count is never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_cap_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e9").await;
    trace::set_cap_for_test(&a, 0);
    let p = Scripted::new(
        PLAIN,
        vec![
            Step::says(FOREIGN).reports(rep(10, 1, Some(5), None)),
            Step::says(DONE).reports(rep(10, 1, Some(5), None)),
        ],
    );
    answered(&kernel, &a, &p, "e9_a").await;
    answered(&kernel, &a, &p, "e9_b").await;
    assert!(traces(&kernel, &a).await.is_empty());
    assert_eq!(ram_count(&a), None);
}

// E10
/// A chat's first pass, D and H: no `model_switched`, and `render_cold`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_pass_no_switch() {
    for (tag, model) in [("tr_e10_d", PLAIN), ("tr_e10_h", QWEN)] {
        let tmp = TempDir::new().unwrap();
        let (kernel, a) = mk_kernel(&tmp, tag).await;
        set_model(&kernel, &a, model).await;
        let p = Scripted::new(model, vec![]);
        answered(&kernel, &a, &p, "e10").await;
        let rows = traces_of(&kernel, &a, "e10").await;
        assert_eq!(names(&rows), ["render_cold"], "{tag}: {rows:?}");
    }
}

// E11
/// A turn cancelled after a retry writes no row, its `retry` included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_turn_writes_no_rows() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e11").await;
    let p = Scripted::new(PLAIN, vec![Step::says(DONE)]);
    answered(&kernel, &a, &p, "e11_a").await;
    let before = traces(&kernel, &a).await;
    p.push(vec![
        Step::says(FOREIGN).reports(rep(10, 1, None, None)),
        Step::hangs("thinking it over"),
    ]);
    let h = spawn_generate(&kernel, &a, &p, "e11_b");
    until_asked(&p, 3).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // the pass ends mid-way: a stop (#795: a pass is no task to drop)
    let _ = as_agent(a.clone(), crate::verbs::stop(&a, None, &kernel)).await;
    let _ = h.await;
    assert_eq!(traces(&kernel, &a).await, before);
    assert!(traces_of(&kernel, &a, "e11_b").await.is_empty());
}

// E12
/// `cache_dropped.why` is `fold`, `cut`, `recall` and `changed`, each from its row of
/// the table (K6, K13, K11, K42); `row` names the dropped row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_why() {
    let mut ev = trace::Events::default();
    // fold (K6)
    {
        let (c, r) = (cache(), Count::new("t"));
        let rows = rows_n(1, 5, 0);
        run(&c, &r, &conv(&refs(&rows), "go"), &Opts::default());
        let mut folded = rows.clone();
        folded[2].1 = "[folded · reply 3]".into();
        let _ = open(
            &c,
            &r,
            &conv(&refs(&folded), "go"),
            &Opts::default().form(3, "anchor"),
        )
        .finish(&mut ev, "turn");
    }
    // cut (K13): the row is gone from the view
    {
        let (c, r) = (cache(), Count::new("t"));
        let rows = rows_n(1, 6, 0);
        let all = refs(&rows);
        run(&c, &r, &conv(&all, "go"), &Opts::default());
        let view: Vec<(i64, &str)> = all.iter().copied().filter(|(i, _)| *i != 2).collect();
        let _ = open(&c, &r, &conv(&view, "go"), &Opts::default()).finish(&mut ev, "turn");
    }
    // recall (K11)
    {
        let (c, r) = (cache(), Count::new("t"));
        run(
            &c,
            &r,
            &conv(&[(1, "q"), (2, "[folded]"), (3, "x")], "go"),
            &Opts::default().form(2, "anchor"),
        );
        let _ = open(
            &c,
            &r,
            &conv(&[(1, "q"), (2, "the whole reply"), (3, "x")], "go"),
            &Opts::default().form(2, "recalled"),
        )
        .finish(&mut ev, "turn");
    }
    // changed (K42): full -> full, other bytes
    {
        let (c, r) = (cache(), Count::new("t"));
        run(
            &c,
            &r,
            &conv(&[(1, "q"), (2, "a")], "go"),
            &Opts::default().form(2, "whole"),
        );
        let _ = open(
            &c,
            &r,
            &conv(&[(1, "q"), (2, "f")], "go"),
            &Opts::default().form(2, "whole"),
        )
        .finish(&mut ev, "turn");
    }
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e12").await;
    let state = crate::state::state_for(&a);
    as_agent(
        a.clone(),
        trace::written(&a, &kernel, &state, "e12", Some(99), ev.into_rows()),
    )
    .await;
    let rows = traces_of(&kernel, &a, "e12").await;
    let dropped: Vec<(Value, Value)> = rows
        .iter()
        .filter(|r| r["event"] == json!("cache_dropped"))
        .map(|r| (r["row"].clone(), r["why"].clone()))
        .collect();
    assert_eq!(
        dropped,
        vec![
            (json!(3), json!("fold")),
            (json!(2), json!("cut")),
            (json!(2), json!("recall")),
            (json!(2), json!("changed")),
        ]
    );
    for r in rows.iter().filter(|r| r["event"] != json!("cache_dropped")) {
        assert_eq!(r["row"], json!(99), "{r}");
    }
}

// E13
/// `retry` rows carry the attempt, the report's first line and the counts it reported; a
/// harness stop writes one `stop` row with the reason and `errors`, one per attempt in
/// order; a model stop writes one with the reason and no `errors`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_and_stop_rows() {
    // retries
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e13_retry").await;
    let p = Scripted::new(
        PLAIN,
        vec![
            Step::says(FOREIGN).unreported(),
            Step::says(FOREIGN).reports(rep(1000, 30, Some(200), None)),
            Step::says(DONE).reports(rep(1000, 5, None, None)),
        ],
    );
    answered(&kernel, &a, &p, "e13_r").await;
    let retries: Vec<Value> = traces_of(&kernel, &a, "e13_r")
        .await
        .into_iter()
        .filter(|r| r["event"] == json!("retry"))
        .collect();
    assert_eq!(retries.len(), 2, "{retries:?}");
    for (i, r) in retries.iter().enumerate() {
        assert_eq!(r["attempt"], json!(i + 1), "{r}");
        let error = r["error"].as_str().unwrap_or_default();
        assert!(!error.is_empty() && !error.contains('\n'), "{r}");
    }
    assert!(
        retries[0].get("prompt").is_none(),
        "no report: {}",
        retries[0]
    );
    assert_eq!(
        (
            &retries[1]["prompt"],
            &retries[1]["completion"],
            &retries[1]["cache_read"],
            &retries[1]["cache_write"]
        ),
        (&json!(800), &json!(30), &json!(200), &json!(0))
    );

    // the harness's stop
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e13_harness").await;
    let n = 1 + crate::constants::CALL_RETRIES;
    let p = Scripted::new(PLAIN, (0..n).map(|_| Step::says(FOREIGN)).collect());
    let (reason, mid) = match generate(&kernel, &a, &p, "e13_h").await {
        Ok(Generation::Stopped { reason, message_id }) => (reason, message_id),
        _ => panic!("not stopped"),
    };
    let rows = traces_of(&kernel, &a, "e13_h").await;
    let mut want = vec!["retry"; n];
    want.push("stop");
    assert_eq!(names(&rows), want, "{rows:?}");
    let stop = rows.last().expect("the stop row");
    assert_eq!(stop["reason"], json!(reason));
    assert_eq!(stop["row"], json!(mid));
    let errors: Vec<Value> = rows[..n].iter().map(|r| r["error"].clone()).collect();
    assert_eq!(stop["errors"], Value::Array(errors));

    // the model's stop
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e13_model").await;
    let p = Scripted::new(PLAIN, vec![Step::says(&stop_call("the file is gone"))]);
    let (_, mid) = answered(&kernel, &a, &p, "e13_m").await;
    let stops: Vec<Value> = traces_of(&kernel, &a, "e13_m")
        .await
        .into_iter()
        .filter(|r| r["event"] == json!("stop"))
        .collect();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0]["reason"], json!("the file is gone"));
    assert_eq!(stops[0]["row"], json!(mid));
    assert!(stops[0].get("errors").is_none(), "{}", stops[0]);
}

// E14
/// `render_cold`/`render_warm`: `rows` is the number of entries the cache rendered, and
/// `kept` the leading messages its chain kept (K1, K2, C1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn render_rows_match_cache() {
    // hermes: rows rendered and cached
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_hermes(&tmp, "tr_e14_h").await;
    let p = Scripted::new(QWEN, vec![]);
    let state = crate::state::state_for(&a);
    let mut events = Vec::new();
    for i in 0..4 {
        let before = state.render_cache.stats().entries;
        let client = format!("e14_{i}");
        answered(&kernel, &a, &p, &client).await;
        let after = state.render_cache.stats().entries;
        let rows = traces_of(&kernel, &a, &client).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let r = &rows[0];
        assert_eq!(r["rows"], json!(after - before), "{client}: {r}");
        let asked = p.asked();
        let kept = if i == 0 {
            0
        } else {
            kept_between(&asked[i - 1], &asked[i])
        };
        assert_eq!(r["kept"], json!(kept), "{client}: {r}");
        events.push(r["event"].as_str().unwrap_or_default().to_string());
    }
    assert_eq!(
        events,
        ["render_cold", "render_cold", "render_warm", "render_warm"]
    );

    // default: the same, its rows mapped and cached like any type's
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e14_d").await;
    let p = Scripted::new(PLAIN, vec![]);
    let state = crate::state::state_for(&a);
    let mut events = Vec::new();
    for i in 0..3 {
        let before = state.render_cache.stats().entries;
        let client = format!("e14d_{i}");
        answered(&kernel, &a, &p, &client).await;
        let after = state.render_cache.stats().entries;
        let rows = traces_of(&kernel, &a, &client).await;
        assert_eq!(rows[0]["rows"], json!(after - before), "{rows:?}");
        events.push(rows[0]["event"].as_str().unwrap_or_default().to_string());
    }
    assert!(state.render_cache.stats().entries > 0);
    assert_eq!(events, ["render_cold", "render_cold", "render_warm"]);
}

// E15
/// A restart with the trace rows pruned, and one with cap 0 before it: a switch across
/// it is not traced, the pass after it writes `render_cold` only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_without_trace() {
    for (tag, zero) in [("tr_e15_pruned", false), ("tr_e15_zero", true)] {
        let tmp = TempDir::new().unwrap();
        let (kernel, a) = mk_kernel(&tmp, tag).await;
        if zero {
            trace::set_cap_for_test(&a, 0);
        }
        let p = Scripted::new(PLAIN, vec![]);
        answered(&kernel, &a, &p, "e15_a").await;
        if zero {
            assert!(traces(&kernel, &a).await.is_empty(), "{tag}");
            trace::set_cap_for_test(&a, crate::constants::TRACE_CAP);
        } else {
            prune_traces(&kernel, &a).await;
        }
        restart(&kernel, &a);
        set_model(&kernel, &a, QWEN).await;
        p.set_model(QWEN);
        answered(&kernel, &a, &p, "e15_b").await;
        let rows = traces_of(&kernel, &a, "e15_b").await;
        assert_eq!(names(&rows), ["render_cold"], "{tag}: {rows:?}");
        assert_eq!(rows[0]["type"], json!("hermes"), "{tag}");
    }
}

// E16
/// A fork of a chat that holds trace rows: the new chat's store has none, the parent's
/// count is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_copies_no_trace() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e16").await;
    let p = Scripted::new(PLAIN, vec![]);
    answered(&kernel, &a, &p, "e16_a").await;
    let (_, last) = answered(&kernel, &a, &p, "e16_b").await;
    let parent = traces(&kernel, &a).await;
    assert_eq!(parent.len(), 2);
    let count = ram_count(&a);
    assert_eq!(count, Some(2));
    let forked = verb(&kernel, &a, json!({"type": "fork", "at": last})).await;
    let child = AgentId::from(forked["id"].as_str().expect("the fork's id"));
    assert!(
        forked["copied"].as_i64().is_some_and(|n| n >= 4),
        "{forked}"
    );
    assert!(traces(&kernel, &child).await.is_empty());
    assert_eq!(ram_count(&a), count);
    assert_eq!(traces(&kernel, &a).await, parent);
}

// E17
/// `render_*` rows carry `kept` and `kept_endpoint` as the cache gives them: equal for
/// the same endpoint while warm; `kept_endpoint` 0 once the model changed (C12-C14).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn render_rows_carry_both() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_hermes(&tmp, "tr_e17").await;
    let p = Scripted::new(QWEN, vec![]);
    for i in 0..3 {
        answered(&kernel, &a, &p, &format!("e17_{i}")).await;
    }
    let asked = p.asked();
    let first = &traces_of(&kernel, &a, "e17_0").await[0];
    assert_eq!(
        (&first["kept"], &first["kept_endpoint"]),
        (&json!(0), &json!(0))
    );
    let third = &traces_of(&kernel, &a, "e17_2").await[0];
    let kept = kept_between(&asked[1], &asked[2]);
    assert!(kept > 0, "{third}");
    assert_eq!(third["kept"], json!(kept));
    assert_eq!(third["kept_endpoint"], json!(kept));
    // another model of the same type: the endpoint's cache is another one
    set_model(&kernel, &a, QWEN_14).await;
    p.set_model(QWEN_14);
    answered(&kernel, &a, &p, "e17_3").await;
    let rows = traces_of(&kernel, &a, "e17_3").await;
    assert_eq!(names(&rows), ["render_warm"], "no switch: {rows:?}");
    let asked = p.asked();
    assert_eq!(rows[0]["kept"], json!(kept_between(&asked[2], &asked[3])));
    assert_eq!(rows[0]["kept_endpoint"], json!(0));
}

// E18
/// A task pass on an endpoint that reports cache counts: inside a turn, one
/// `prefix_cache` row with `mode: "task"` among the turn's rows; outside a turn, none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_rows_carry_mode() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e18").await;
    let p = Scripted::new(
        PLAIN,
        vec![
            Step::says(DONE),
            // the owed recall's compose, then attempt 1
            Step::says("{}").reports(rep(300, 30, Some(100), None)),
            Step::says(DONE).reports(rep(1000, 10, None, None)),
        ],
    );
    answered(&kernel, &a, &p, "e18_a").await;
    *crate::state::state_for(&a).recall_due.lock().unwrap() = true;
    answered(&kernel, &a, &p, "e18_in").await;
    assert_eq!(p.asked().len(), 3, "the recall asked the model");
    let rows = traces_of(&kernel, &a, "e18_in").await;
    let task: Vec<&Value> = rows.iter().filter(|r| r["mode"] == json!("task")).collect();
    assert_eq!(task.len(), 1, "{rows:?}");
    assert_eq!(task[0]["event"], json!("prefix_cache"));
    assert_eq!(task[0]["cache_read"], json!(100));
    assert!(rows.iter().any(|r| r["mode"] == json!("turn")), "{rows:?}");

    // outside a turn: the fold summarizer on a verb's pass
    let before = traces(&kernel, &a).await;
    p.push(vec![Step::says("SUMMARY").reports(rep(
        40,
        4,
        Some(20),
        Some(2),
    ))]);
    let state = crate::state::state_for(&a);
    let msgs = vec![json!({"role": "user", "id": 1, "content": "go"})];
    let summary = as_agent(a.clone(), async {
        let pass = Pass::new(Arc::clone(&p) as Arc<dyn Provider>, &a, &state, &kernel).await;
        crate::strategies::summarize(&pass, &msgs, None).await.0
    })
    .await;
    assert_eq!(summary, "SUMMARY");
    assert_eq!(
        traces(&kernel, &a).await,
        before,
        "no trace row outside a turn"
    );
    answered(&kernel, &a, &p, "e18_next").await;
    let next = traces_of(&kernel, &a, "e18_next").await;
    assert!(next.iter().all(|r| r["mode"] == json!("turn")), "{next:?}");
}

// E19
/// A crashed pass, at each point of RS2, RS3 and RS4: the chat's state dropped with no
/// end-of-turn append. No trace row of that send is stored; the rows of earlier turns are
/// untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_pass_writes_no_rows() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "tr_e19").await;
    let p = Scripted::new(PLAIN, vec![Step::says(DONE)]);
    answered(&kernel, &a, &p, "e19_ok").await;
    let before = traces(&kernel, &a).await;
    assert_eq!(before.len(), 1);
    let points: Vec<(&str, Vec<Step>)> = vec![
        // RS2: the view built, the request never answered
        ("mid_pass", vec![Step::stalls()]),
        // RS3: tokens arrived, no reply yet
        ("mid_stream", vec![Step::hangs("half an answer")]),
        // RS4, m = 2: while the re-ask streams, and before its request
        (
            "retry2_stream",
            vec![Step::says(FOREIGN), Step::hangs("again")],
        ),
        ("retry2_request", vec![Step::says(FOREIGN), Step::stalls()]),
        // RS4, m = 3
        (
            "retry3_stream",
            vec![
                Step::says(FOREIGN),
                Step::says(FOREIGN),
                Step::hangs("again"),
            ],
        ),
        (
            "retry3_request",
            vec![Step::says(FOREIGN), Step::says(FOREIGN), Step::stalls()],
        ),
    ];
    for (point, steps) in points {
        let n = steps.len();
        let asked = p.asked().len();
        p.push(steps);
        let client = format!("e19_{point}");
        let h = spawn_generate(&kernel, &a, &p, &client);
        until_asked(&p, asked + n).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // the pass ends mid-way: a stop (#795: a pass is no task to drop)
        let _ = as_agent(a.clone(), crate::verbs::stop(&a, None, &kernel)).await;
        let _ = h.await;
        crate::state::drop_state(&a);
        assert!(traces_of(&kernel, &a, &client).await.is_empty(), "{point}");
        assert_eq!(traces(&kernel, &a).await, before, "{point}");
    }
}

// E20
/// A clean turn leaves exactly its events, one row each: a cold turn, a warm turn with
/// one retry, a turn with a drop. Then three restarts with a burst of twice the cap
/// between them: after every restart the count is read once and stays within the cap;
/// the survivors are the newest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_turn_exact_rows_cap_across_restarts() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_hermes(&tmp, "tr_e20").await;
    let p = Scripted::new(QWEN, vec![]);
    let state = crate::state::state_for(&a);
    // a cold turn
    let (_, mid) = answered(&kernel, &a, &p, "e20_cold").await;
    let rows = traces_of(&kernel, &a, "e20_cold").await;
    assert_eq!(names(&rows), ["render_cold"], "{rows:?}");
    assert_eq!(
        (&rows[0]["rows"], &rows[0]["kept"], &rows[0]["row"]),
        (&json!(0), &json!(0), &json!(mid))
    );
    answered(&kernel, &a, &p, "e20_second").await;
    assert_eq!(
        names(&traces_of(&kernel, &a, "e20_second").await),
        ["render_cold"],
        "no entry of the type was hit yet"
    );
    // a warm turn with one retry: the internal form is foreign to hermes
    p.push(vec![Step::raw(DONE), Step::says(DONE)]);
    answered(&kernel, &a, &p, "e20_retry").await;
    let rows = traces_of(&kernel, &a, "e20_retry").await;
    assert_eq!(names(&rows), ["retry", "render_warm"], "{rows:?}");
    assert_eq!(rows[0]["attempt"], json!(1));
    // a turn with a drop: after a clear, every cached row is cut
    let cached: Vec<i64> = state
        .render_cache
        .stats()
        .keys
        .into_iter()
        .filter(|(t, _)| t == super::hermes::NAME)
        .map(|(_, id)| id)
        .collect();
    assert!(!cached.is_empty());
    let clear = verb(&kernel, &a, json!({"type": "context_clear"})).await;
    assert!(clear.get("error").is_none(), "{clear}");
    answered(&kernel, &a, &p, "e20_drop").await;
    let rows = traces_of(&kernel, &a, "e20_drop").await;
    let mut want = vec!["render_cold".to_string()];
    want.extend(cached.iter().map(|_| "cache_dropped".to_string()));
    assert_eq!(names(&rows), want, "{rows:?}");
    let dropped: Vec<(Value, Value)> = rows[1..]
        .iter()
        .map(|r| (r["row"].clone(), r["why"].clone()))
        .collect();
    let expected: Vec<(Value, Value)> = cached.iter().map(|id| (json!(id), json!("cut"))).collect();
    assert_eq!(dropped, expected);
    // no row of the chat's store but its events
    assert_eq!(
        traces(&kernel, &a).await.len(),
        1 + 1 + 2 + want.len(),
        "exactly the events, one row each"
    );

    // three restarts, a burst of twice the cap between them
    let cap = 5;
    trace::set_cap_for_test(&a, cap);
    let mut seen = ids(&traces(&kernel, &a).await);
    for round in 0..3 {
        burst(&kernel, &a, &format!("e20_burst{round}"), 2 * cap).await;
        seen.extend(ids(&traces(&kernel, &a).await));
        restart(&kernel, &a);
        assert_eq!(
            ram_count(&a),
            None,
            "round {round}: the count goes with the state"
        );
        answered(&kernel, &a, &p, &format!("e20_after{round}")).await;
        let rows = traces(&kernel, &a).await;
        seen.extend(ids(&rows));
        seen.sort_unstable();
        seen.dedup();
        assert!(rows.len() <= cap, "round {round}: {}", rows.len());
        assert_eq!(ram_count(&a), Some(rows.len()), "round {round}");
        assert_eq!(
            ids(&rows),
            seen[seen.len() - rows.len()..].to_vec(),
            "round {round}"
        );
    }
}

// ─── M19 ─────────────────────────────────────────────────────────────────────

// M19
/// A switch after a restart: the type comes from the newest row's `attributes.model`,
/// the render is cold, and `model_switched` is written because the newest trace row
/// names another type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switch_seen_after_restart() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = mk_kernel(&tmp, "tr_m19_base").await;
    let a = chat_on(
        &kernel,
        "tr_m19_chat",
        fantastic_ram_store::test_support::MEMORY,
        None,
    )
    .await;
    // no record model, nothing seen yet: the first pass is `default`; the endpoint then
    // names a hermes model, and the stored row keeps it
    let p = Scripted::new(PLAIN, vec![Step::says(DONE).named_model(QWEN)]);
    answered(&kernel, &a, &p, "m19_a").await;
    let first = traces_of(&kernel, &a, "m19_a").await;
    assert_eq!(first[0]["type"], json!("default"), "{first:?}");
    restart(&kernel, &a);
    answered(&kernel, &a, &p, "m19_b").await;
    let rows = traces_of(&kernel, &a, "m19_b").await;
    assert_eq!(names(&rows), ["model_switched", "render_cold"], "{rows:?}");
    assert_eq!(rows[0]["from"], json!("default"));
    assert_eq!(rows[0]["to"], json!("hermes"));
    assert_eq!(rows[0]["model"], json!(QWEN));
    assert_eq!(rows[1]["type"], json!("hermes"));
}

/// A hermes call whose JSON the reader completed with closers only is a `repaired`
/// event of its attempt, naming the closers; a call that read as written is none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repaired_call_traced() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_hermes(&tmp, "tr_repaired").await;
    let p = Scripted::new(
        QWEN,
        vec![
            Step::says("ok\n<tool_call>\n{\"name\": \"done\", \"arguments\": {}\n</tool_call>"),
            Step::says("ok\n<tool_call>\n{\"name\": \"done\", \"arguments\": {}}\n</tool_call>"),
        ],
    );
    answered(&kernel, &a, &p, "rp_a").await;
    let rows = traces_of(&kernel, &a, "rp_a").await;
    assert_eq!(names(&rows), ["repaired", "render_cold"], "{rows:?}");
    assert_eq!(rows[0]["attempt"], json!(1), "{}", rows[0]);
    assert_eq!(rows[0]["added"], json!("}"), "{}", rows[0]);
    assert_eq!(rows[0]["type"], json!(super::hermes::NAME), "{}", rows[0]);
    assert_eq!(rows[0]["mode"], json!("turn"), "{}", rows[0]);
    answered(&kernel, &a, &p, "rp_b").await;
    let rows = traces_of(&kernel, &a, "rp_b").await;
    assert!(!names(&rows).contains(&"repaired".to_string()), "{rows:?}");
}
