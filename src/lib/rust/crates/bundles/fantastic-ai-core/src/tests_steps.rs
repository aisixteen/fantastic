//! #795 B3: AN AI AGENT'S PLUMBING CALLS ARE PASS STEPS ([`crate::steps`]). Each test
//! holds one of the agent's calls at a GATED memory store (the ram store, with a gate on
//! the verbs a test names) and checks what the agent does meanwhile: nothing of its own
//! waits — no `pass` task sleeps on the store, a `send` is not acked before its row is
//! stored, an answer's calls do not go out before the answer row is — and the held
//! call's settle is the step that goes on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::agent_loop::BackendConfig;
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use crate::tests_decl::chat_decls;
use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{Agent, AgentId, Blob, Item, Kernel};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

const HANDLER: &str = "steps_mock.tools";
const PROBE: &str = "steps_probe.tools";

const CFG: BackendConfig = BackendConfig {
    name: "steps_mock",
    model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
};

// ── the gated memory store ───────────────────────────────────────────────────

/// Which of an agent's store calls a gate holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum What {
    /// Every `read`.
    Read,
    /// Every `append`.
    Append,
    /// An `append` that carries an assistant row (a pass's answer row).
    Answer,
}

/// A gate on ONE agent's store calls: how many it holds, and whether it is open.
struct Gate {
    what: What,
    held: AtomicUsize,
    open: tokio::sync::watch::Sender<bool>,
}

impl Gate {
    fn holds(&self, call: &fantastic_ram_store::verbs::Call) -> bool {
        use fantastic_ram_store::verbs::Call;
        match (self.what, call) {
            (What::Read, Call::Read(_)) | (What::Append, Call::Append(_)) => true,
            (What::Answer, Call::Append(a)) => a
                .rows
                .iter()
                .any(|r| r.rest.get("role").and_then(Value::as_str) == Some("assistant")),
            _ => false,
        }
    }
}

static GATES: OnceLock<Mutex<HashMap<String, Arc<Gate>>>> = OnceLock::new();

fn gates() -> &'static Mutex<HashMap<String, Arc<Gate>>> {
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Gate `agent`'s store calls of `what`: held until [`Gate::open`] is set.
fn gate(agent: &str, what: What) -> Arc<Gate> {
    let (open, _) = tokio::sync::watch::channel(false);
    let g = Arc::new(Gate {
        what,
        held: AtomicUsize::new(0),
        open,
    });
    gates().lock().unwrap().insert(agent.to_string(), Arc::clone(&g));
    g
}

/// The ram store, with each caller's gated calls held in its handler until the gate
/// opens (a slow store, as a store on a remote disk is).
struct Gated(fantastic_ram_store::RamStoreBundle);

impl Gated {
    async fn hold(call: &fantastic_ram_store::verbs::Call) {
        let caller = fantastic_kernel::current_origin().map(|a| a.as_str().to_string());
        let g = caller.and_then(|c| gates().lock().unwrap().get(&c).cloned());
        let Some(g) = g.filter(|g| g.holds(call)) else {
            return;
        };
        g.held.fetch_add(1, Ordering::SeqCst);
        let mut rx = g.open.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[async_trait]
impl Handler for Gated {
    type Call = fantastic_ram_store::verbs::Call;
    fn name(&self) -> &str {
        self.0.name()
    }
    fn man(&self) -> &'static str {
        self.0.man()
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        self.0.verbs()
    }
    fn dynamic_verbs(&self, a: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        self.0.dynamic_verbs(a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        self.0.record()
    }
    async fn handle(&self, agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Self::hold(&call).await;
        self.0.handle(agent_id, call, kernel).await
    }
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        Self::hold(&call).await;
        self.0.handle_binary(agent_id, call, body, kernel).await
    }
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        self.0.on_delete(agent_id, kernel).await
    }
}

// ── the chat and its mock model ──────────────────────────────────────────────

/// Every request one agent's model was served.
#[derive(Default)]
struct Served {
    requests: Mutex<Vec<Vec<Value>>>,
}

static SERVED: OnceLock<Mutex<HashMap<String, Arc<Served>>>> = OnceLock::new();

fn served_for(agent: &str) -> Arc<Served> {
    let map = SERVED.get_or_init(|| Mutex::new(HashMap::new()));
    Arc::clone(map.lock().unwrap().entry(agent.to_string()).or_default())
}

fn served(agent: &str) -> usize {
    served_for(agent).requests.lock().unwrap().len()
}

/// The model: its first answer is `<agent>`'s (a `_pings` agent sends its probe a
/// `ping`), every later one `done`.
struct Mock {
    served: Arc<Served>,
    first: String,
}

#[async_trait]
impl Provider for Mock {
    async fn chat(&self, messages: &[Value], _max_tokens: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let text = {
            let mut reqs = self.served.requests.lock().unwrap();
            let text = if reqs.is_empty() {
                self.first.clone()
            } else {
                r#"<call name="done"></call>"#.to_string()
            };
            reqs.push(messages.to_vec());
            text
        };
        let ev: Vec<Result<ProviderEvent, ProviderError>> = vec![Ok(ProviderEvent::Token(text))];
        Ok(Box::pin(futures_util::stream::iter(ev)))
    }
    fn model(&self) -> String {
        "steps-model".to_string()
    }
}

fn first_answer(agent: &str) -> String {
    match agent.strip_suffix("_pings") {
        Some(base) => {
            let mut args = Map::new();
            args.insert("target_id".into(), json!(format!("{base}_probe")));
            args.insert("payload".into(), json!({"type": "ping"}));
            format!("{}<call name=\"done\"></call>", crate::tool_parse::render_call("send", &args))
        }
        None => r#"<call name="done"></call>"#.to_string(),
    }
}

struct MockBundle;

#[async_trait]
impl Handler for MockBundle {
    chat_decls!();
    fn name(&self) -> &str {
        CFG.name
    }
    async fn handle(&self, agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let served = served_for(agent_id.as_str());
        let first = first_answer(agent_id.as_str());
        let reply = crate::verbs::handle(
            agent_id,
            call,
            kernel,
            CFG,
            move |_client| {
                Arc::new(Mock {
                    served: Arc::clone(&served),
                    first: first.clone(),
                }) as Arc<dyn Provider>
            },
            || json!({"id": "steps_mock"}),
        )
        .await?;
        Ok(Some(reply))
    }
}

/// The probe an answer pings: it counts its pings.
struct Probe;

static PINGS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

fn pings(probe: &str) -> usize {
    PINGS.get_or_init(Default::default).lock().unwrap().get(probe).copied().unwrap_or(0)
}

/// `ping` of the probe.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Ping {}

mod probe_verbs {
    use super::Ping;
    fantastic_kernel::verbs! { "ping" => Ping }
}

#[async_trait]
impl Handler for Probe {
    type Call = probe_verbs::Call;
    fn name(&self) -> &str {
        "steps_probe"
    }
    fn man(&self) -> &'static str {
        "A probe that counts its pings."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        probe_verbs::VERBS
    }
    fn dynamic_verbs(&self, a: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        probe_verbs::dynamic_verbs(a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &crate::tests_decl::any::RECORD
    }
    async fn handle(&self, agent_id: &AgentId, call: Self::Call, _kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        use probe_verbs::Call;
        let reply = match call {
            Call::Reflect(_) => json!({"id": agent_id.as_str()}),
            Call::Ping(_) => {
                *PINGS.get_or_init(Default::default).lock().unwrap().entry(agent_id.as_str().to_string()).or_default() += 1;
                json!({"pong": true})
            }
        };
        Ok(Some(reply))
    }
}

async fn create(kernel: &Arc<Kernel>, create: Value) {
    let r = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
}

/// A kernel with the gated store, the mock chat `agent` (a line on the store, on
/// `cli` and on its probe) and its probe `<base>_probe`.
async fn setup(tmp: &TempDir, agent: &str, store: Option<&str>) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    kernel.bundles.register(HANDLER, MockBundle);
    kernel.bundles.register(PROBE, Probe);
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        Gated(fantastic_ram_store::RamStoreBundle::default()),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let base = agent.strip_suffix("_pings").unwrap_or(agent);
    let probe = format!("{base}_probe");
    create(&kernel, json!({"type": "create_agent", "parent_id": "kernel", "handler_module": PROBE, "id": probe})).await;
    let can = json!({"cli": {"call": ["*"]}, probe.clone(): {"call": ["*"]}});
    let memory = fantastic_ram_store::test_support::memory_meta(&kernel, agent, can).await;
    let mut c = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER,
        "id": agent,
        "model": "steps-model",
        "context_window": 32768,
    });
    for (k, v) in memory {
        c[k] = v;
    }
    if let Some(s) = store {
        c["memory_store_id"] = json!(s);
    }
    create(&kernel, c).await;
    kernel
}

/// `cond` within about 5 seconds.
async fn until(cond: impl Fn() -> bool) -> bool {
    for _ in 0..250 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

/// The agent's own tasks in `ps` now: `(verb, id)`.
fn own_tasks(kernel: &Kernel, agent: &str) -> Vec<(String, i64)> {
    kernel
        .tasks
        .rows()
        .iter()
        .filter(|r| r["agent"] == agent)
        .map(|r| (r["verb"].as_str().unwrap_or("").to_string(), r["id"].as_i64().unwrap_or(0)))
        .collect()
}

/// A `send` to `agent` from the operator, its settle to come.
fn send_bg(kernel: &Arc<Kernel>, agent: &str, text: &str) -> tokio::task::JoinHandle<fantastic_kernel::Outcome> {
    let (k, item) = (
        Arc::clone(kernel),
        Item::new(AgentId::from(agent), json!({"type": "send", "text": text, "client_id": "steps"})),
    );
    tokio::spawn(async move { k.operator_send(vec![item]).await.remove(0) })
}

/// While the pass's HISTORY READ is held at the store, the pass holds no task of its
/// own: its step ended when the read went out. The read's settle is the next step: the
/// pass goes on and asks its model.
#[tokio::test]
async fn a_pass_holds_no_task_while_its_history_read_is_out() {
    let tmp = TempDir::new().unwrap();
    let a = "steps_t1";
    let kernel = setup(&tmp, a, None).await;
    let g = gate(a, What::Read);
    let acked = send_bg(&kernel, a, "hello").await.unwrap();
    assert_eq!(acked.result["accepted"], json!(true), "{}", acked.result);
    assert!(until(|| g.held.load(Ordering::SeqCst) >= 1).await, "the pass never read its history");
    let state = crate::state::state_for(&AgentId::from(a));
    assert!(state.life.lock().unwrap().running, "the pass is not in flight");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tasks = own_tasks(&kernel, a);
    assert!(
        !tasks.iter().any(|(v, _)| v == "pass"),
        "a pass task waits on the store: {tasks:?}"
    );
    assert_eq!(served(a), 0, "the model was asked before the history was read");
    let _ = g.open.send(true);
    assert!(until(|| served(a) == 1).await, "the read's settle did not go on to the model");
}

/// A `send` is ACKED ONCE ITS ROW IS STORED (#795): while the store holds the append the
/// call stays open; its settle is `{accepted, re}`, `re` the row the store took.
#[tokio::test]
async fn a_send_is_acked_once_its_row_is_stored() {
    let tmp = TempDir::new().unwrap();
    let a = "steps_t2";
    let kernel = setup(&tmp, a, None).await;
    let g = gate(a, What::Append);
    let settle = send_bg(&kernel, a, "store me");
    assert!(until(|| g.held.load(Ordering::SeqCst) >= 1).await, "the row never went to the store");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!settle.is_finished(), "the send was acked before its row was stored");
    let _ = g.open.send(true);
    let out = tokio::time::timeout(Duration::from_secs(5), settle)
        .await
        .expect("the send never settled")
        .unwrap();
    assert_eq!(out.result["accepted"], json!(true), "{}", out.result);
    let re = out.result["re"].as_i64().expect("re");
    let rows = fantastic_kernel::test_support::as_agent(AgentId::from(a), async {
        crate::memory::load_history(&AgentId::from(a), &kernel, false).await
    })
    .await
    .expect("history");
    assert!(
        rows.iter().any(|r| r["id"] == re && r["content"] == "store me"),
        "the acked row is not the stored one: {rows:?}"
    );
}

/// A store that cannot take the row (it does not exist) FAILS the send as the store's
/// raise, and nothing is put on the unread list.
#[tokio::test]
async fn a_store_that_cannot_take_the_row_fails_the_send_as_a_raise() {
    let tmp = TempDir::new().unwrap();
    let a = "steps_t3";
    let kernel = setup(&tmp, a, Some("no_such_store")).await;
    let out = tokio::time::timeout(Duration::from_secs(5), send_bg(&kernel, a, "lost"))
        .await
        .expect("the send never settled")
        .unwrap();
    assert_eq!(out.status, fantastic_kernel::Status::Failed, "{}", out.result);
    assert_eq!(out.result["raised"], json!(true), "{}", out.result);
    assert!(
        out.result["error"].as_str().is_some_and(|e| e.starts_with("MemoryStoreError: ")),
        "{}",
        out.result
    );
    assert_eq!(crate::life::unread_count(&AgentId::from(a)), 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(served(a), 0, "a pass ran over a message never stored");
}

/// The ANSWER ROW is written before the answer's calls go out: while the store holds
/// it, the probe the answer pings has not been called; once it took the row, the call
/// goes out.
#[tokio::test]
async fn an_answers_calls_go_out_once_its_row_is_stored() {
    let tmp = TempDir::new().unwrap();
    let a = "steps_t4_pings";
    let kernel = setup(&tmp, a, None).await;
    let g = gate(a, What::Answer);
    let _ = send_bg(&kernel, a, "ping your probe").await.unwrap();
    assert!(until(|| g.held.load(Ordering::SeqCst) >= 1).await, "the answer row never went to the store");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(pings("steps_t4_probe"), 0, "a call went out before its answer row was stored");
    let state = crate::state::state_for(&AgentId::from(a));
    assert!(state.life.lock().unwrap().running, "the pass ended before its row was stored");
    let _ = g.open.send(true);
    assert!(until(|| pings("steps_t4_probe") == 1).await, "the answer's call never went out");
}

/// A message the KERNEL HANDS OVER (bookkeeping: an `await_set` wake, run inside the
/// dispatch whose report completed the set) answers no call: nothing is deferred or
/// settled, the row is stored by its steps, and the pass it wakes reads it.
#[tokio::test]
async fn a_message_the_kernel_hands_over_is_stored_and_read() {
    let tmp = TempDir::new().unwrap();
    let a = "steps_t5";
    let kernel = setup(&tmp, a, None).await;
    let agent = kernel.resolve_target(a).expect("the chat");
    let handed = fantastic_kernel::test_support::bookkeeping(
        &kernel,
        &agent,
        json!({"type": "send", "text": "handed over"}),
    );
    let out = fantastic_kernel::send::as_operator(handed).await;
    assert_eq!(out.status, fantastic_kernel::Status::Completed, "{}", out.result);
    assert!(until(|| served(a) == 1).await, "the handed-over message woke no pass");
    let read = Value::Array(served_for(a).requests.lock().unwrap()[0].clone()).to_string();
    assert!(read.contains("handed over"), "{read}");
}

/// A call to `agent` from the operator, its settle to come.
fn call_bg(kernel: &Arc<Kernel>, agent: &str, payload: Value) -> tokio::task::JoinHandle<fantastic_kernel::Outcome> {
    let (k, item) = (Arc::clone(kernel), Item::new(AgentId::from(agent), payload));
    tokio::spawn(async move { k.operator_send(vec![item]).await.remove(0) })
}

/// A MEMORY VERB IS ANSWERED BY ITS STEPS (#795): its handler defers and sends its store
/// read; while the store holds the read the call stays open, and the read's settle is
/// the step that goes on and answers it — for each verb that reads the agent's store.
#[tokio::test]
async fn a_memory_verb_is_answered_by_the_settle_of_its_store_read() {
    let verbs = [
        (json!({"type": "history"}), "rows"),
        (json!({"type": "recall"}), "messages"),
        (json!({"type": "grep", "query": "x", "ids": [1]}), "matches"),
        (json!({"type": "fork_points"}), "points"),
        (json!({"type": "pause"}), "paused"),
        (json!({"type": "context_forget"}), "pruned"),
        (json!({"type": "context_clear"}), "cleared"),
    ];
    for (i, (payload, key)) in verbs.into_iter().enumerate() {
        let tmp = TempDir::new().unwrap();
        let a = format!("steps_t6_{i}");
        let kernel = setup(&tmp, &a, None).await;
        let g = gate(&a, What::Read);
        let verb = payload["type"].clone();
        let settle = call_bg(&kernel, &a, payload);
        assert!(
            until(|| g.held.load(Ordering::SeqCst) >= 1).await,
            "{verb}: the store read never went out"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!settle.is_finished(), "{verb} was answered before its read settled");
        let _ = g.open.send(true);
        let out = tokio::time::timeout(Duration::from_secs(5), settle)
            .await
            .unwrap_or_else(|_| panic!("{verb} never settled"))
            .unwrap();
        assert_eq!(out.status, fantastic_kernel::Status::Completed, "{verb}: {}", out.result);
        assert!(out.result.get(key).is_some(), "{verb}: {}", out.result);
    }
}

/// A VERB'S STORE FAILURE IS ITS RAISE, from its step: a store that does not exist fails
/// the call as a `MemoryStoreError`, as an awaited read did.
#[tokio::test]
async fn a_memory_verb_whose_store_is_gone_fails_as_a_raise() {
    let tmp = TempDir::new().unwrap();
    let a = "steps_t7";
    let kernel = setup(&tmp, a, Some("no_such_store")).await;
    let out = tokio::time::timeout(Duration::from_secs(5), call_bg(&kernel, a, json!({"type": "history"})))
        .await
        .expect("history never settled")
        .unwrap();
    assert_eq!(out.status, fantastic_kernel::Status::Failed, "{}", out.result);
    assert_eq!(out.result["raised"], json!(true), "{}", out.result);
    assert!(
        out.result["error"].as_str().is_some_and(|e| e.starts_with("MemoryStoreError: ")),
        "{}",
        out.result
    );
}

// ── the provider side (#795 C2) ──────────────────────────────────────────────

/// A credential store whose `get` is held until [`CREDS_OPEN`] opens; it answers the
/// key `k-steps` for any ref.
struct HeldCreds;

static CREDS_OPEN: OnceLock<tokio::sync::watch::Sender<bool>> = OnceLock::new();
static CREDS_HELD: AtomicUsize = AtomicUsize::new(0);

fn creds_gate() -> &'static tokio::sync::watch::Sender<bool> {
    CREDS_OPEN.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// `get` of the held credential store.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct Get {
    /// The ref.
    #[serde(rename = "ref")]
    _reference: String,
    /// The host the read is pinned to.
    #[serde(rename = "host")]
    _host: Option<String>,
}

mod creds_verbs {
    use super::Get;
    fantastic_kernel::verbs! { "get" => Get }
}

#[async_trait]
impl Handler for HeldCreds {
    type Call = creds_verbs::Call;
    fn name(&self) -> &str {
        "steps_creds"
    }
    fn man(&self) -> &'static str {
        "A credential store that answers once its gate opens."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        creds_verbs::VERBS
    }
    fn dynamic_verbs(&self, a: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        creds_verbs::dynamic_verbs(a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &crate::tests_decl::any::RECORD
    }
    async fn handle(&self, agent_id: &AgentId, call: Self::Call, _kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        use creds_verbs::Call;
        Ok(Some(match call {
            Call::Reflect(_) => json!({"id": agent_id.as_str()}),
            Call::Get(_) => {
                CREDS_HELD.fetch_add(1, Ordering::SeqCst);
                let mut rx = creds_gate().subscribe();
                while !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                json!({"fields": {"api_key": "k-steps"}})
            }
        }))
    }
}

/// A model that answers with the key its request was given for its endpoint.
struct Keyed(Arc<dyn crate::credentials::KeySource>);

#[async_trait]
impl Provider for Keyed {
    async fn chat(&self, _messages: &[Value], _max_tokens: Option<i64>) -> Result<ProviderStream, ProviderError> {
        let key = self.0.key("http://steps.test/v1").await.map_err(|e| ProviderError::raised(e.0))?;
        let ev: Vec<Result<ProviderEvent, ProviderError>> = vec![Ok(ProviderEvent::Token(format!("key={key:?}")))];
        Ok(Box::pin(futures_util::stream::iter(ev)))
    }
    fn model(&self) -> String {
        "keyed-1".to_string()
    }
}

/// A provider agent: every verb through [`crate::remote_provider::handle`], its key read
/// from its record's `credentials`.
struct StepsProvider;

#[async_trait]
impl Handler for StepsProvider {
    type Call = crate::declared::provider::Call;
    fn name(&self) -> &str {
        "steps_provider"
    }
    fn man(&self) -> &'static str {
        "A provider whose key and settings are steps."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        crate::declared::provider::VERBS
    }
    fn dynamic_verbs(&self, a: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        crate::declared::provider::dynamic_verbs(a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &crate::tests_decl::any::RECORD
    }
    async fn handle(&self, agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        crate::remote_provider::handle(
            agent_id,
            call,
            kernel,
            Some(crate::remote_provider::Keys { backend: "steps_provider", required: false }),
            None,
            |_meta, key| Arc::new(Keyed(key)) as Arc<dyn Provider>,
            crate::remote_provider::NO_LIVE,
            || json!({"id": "steps_provider"}),
        )
        .await
    }
}

/// A kernel with the gated store, the held credential store `steps_creds` and the
/// provider agent `provider` (its settings in the store, its key in `steps_creds`).
async fn provider_setup(tmp: &TempDir, provider: &str) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    kernel.bundles.register("steps_provider.tools", StepsProvider);
    kernel.bundles.register("steps_creds.tools", HeldCreds);
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        Gated(fantastic_ram_store::RamStoreBundle::default()),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(AgentId::from("kernel_state"), None, None, Map::new(), tmp.path().join(".fantastic"), false);
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    if kernel.resolve_target("steps_creds").is_none() {
        create(&kernel, json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "steps_creds.tools", "id": "steps_creds"})).await;
    }
    let can = json!({"steps_creds": {"call": ["get"]}});
    let memory = fantastic_ram_store::test_support::memory_meta(&kernel, provider, can).await;
    let mut c = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": "steps_provider.tools",
        "id": provider,
        "endpoint": "http://steps.test/v1",
        "credentials": {"store": "steps_creds", "ref": "steps"},
    });
    for (k, v) in memory {
        c[k] = v;
    }
    create(&kernel, c).await;
    kernel
}

/// A PROVIDER'S KEY READ IS A STEP (#795): its `complete` defers and sends the read to
/// the credential store; while the store holds it the call stays open, and the read's
/// settle runs the pass with that key, pinned to the record's endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_providers_key_read_is_a_step_and_its_settle_runs_the_pass() {
    let tmp = TempDir::new().unwrap();
    let p = "steps_p1";
    let kernel = provider_setup(&tmp, p).await;
    let before = CREDS_HELD.load(Ordering::SeqCst);
    let settle = call_bg(
        &kernel,
        p,
        json!({"type": "complete", "rid": "r1", "model": "keyed-1",
               "messages": [{"role": "user", "content": "hi"}]}),
    );
    assert!(
        until(|| CREDS_HELD.load(Ordering::SeqCst) > before).await,
        "the key was never read"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!settle.is_finished(), "complete was answered before its key was read");
    let _ = creds_gate().send(true);
    let out = tokio::time::timeout(Duration::from_secs(5), settle)
        .await
        .expect("complete never settled")
        .unwrap();
    assert_eq!(out.status, fantastic_kernel::Status::Completed, "{}", out.result);
    assert_eq!(out.result["text"], json!("key=Some(\"k-steps\")"), "{}", out.result);
}

/// A PROVIDER'S SETTINGS ARE STEPS (#795): `set_default_model` stays open while the
/// store holds its write and settles from the write's settle; `models` reads the default
/// the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_providers_setting_write_and_read_are_steps() {
    let tmp = TempDir::new().unwrap();
    let p = "steps_p2";
    let kernel = provider_setup(&tmp, p).await;
    let g = gate(p, What::Append);
    let settle = call_bg(&kernel, p, json!({"type": "set_default_model", "model": "m-2"}));
    assert!(until(|| g.held.load(Ordering::SeqCst) >= 1).await, "the setting never went to the store");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!settle.is_finished(), "set_default_model was answered before its write settled");
    let _ = g.open.send(true);
    let out = tokio::time::timeout(Duration::from_secs(5), settle)
        .await
        .expect("set_default_model never settled")
        .unwrap();
    assert_eq!(out.result, json!({"default": "m-2"}));
    let g = gate(p, What::Read);
    let settle = call_bg(&kernel, p, json!({"type": "models"}));
    assert!(until(|| g.held.load(Ordering::SeqCst) >= 1).await, "the default was never read");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!settle.is_finished(), "models was answered before its read settled");
    let _ = g.open.send(true);
    let out = tokio::time::timeout(Duration::from_secs(5), settle)
        .await
        .expect("models never settled")
        .unwrap();
    assert_eq!(out.result["default"], json!("m-2"), "{}", out.result);
}
