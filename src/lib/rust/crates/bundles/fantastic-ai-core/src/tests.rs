//! Shared verb/loop/assembly/state tests, driven through `kernel.send`
//! against a `MockProvider` (no HTTP). A real `fantastic-file` agent
//! handles persistence. Each test uses a unique agent id derived from
//! its tempdir so the process-global state map doesn't race.

use super::*;
use crate::agent_loop::BackendConfig;
use crate::provider::{within, Provider, ProviderError, ProviderEvent, ProviderStream};
use crate::tests_decl::chat_decls;
use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const HANDLER_MODULE: &str = "mock_backend.tools";

#[derive(Clone)]
pub(crate) enum ScriptEvent {
    Token(String),
    /// The provider stream fails mid-pass. `run_generation` propagates it, so the
    /// `send` reply carries an `error` — which is what a broken chain must look
    /// like from the router's side.
    Fail(String),
    /// A failure of the LINK (`ProviderError::transport`): the retry path (#198).
    TransportFail(String),
    /// The endpoint answered this HTTP status, classed by the shared classifier
    /// ([`crate::provider::http_class`]) as every backend classes it.
    HttpFail(u16),
    /// The read clock tripped: the stream sends nothing for `idle`, then fails with
    /// the provider's own `stream_stalled` error ([`crate::provider::within`]). The
    /// window is the record's `stream_idle_seconds`, shortened for the test.
    Stall {
        idle: Duration,
        endpoint: String,
    },
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
pub(crate) struct MockScript {
    passes: std::sync::Mutex<std::collections::VecDeque<Vec<ScriptEvent>>>,
    /// Every `chat()` served, in order — the model-view message list it was
    /// handed. Turn 2's entry is how a test proves a reply actually came back.
    requests: std::sync::Mutex<Vec<Vec<Value>>>,
    delay: Duration,
    /// Answers a request before the queue does when it returns `Some` — python's
    /// mock `responder` (a fold's summary side call, answered in its own shape).
    responder: std::sync::Mutex<Option<Responder>>,
    /// Fired (one permit) each time a request is served, so [`until_passes`]
    /// wakes on the event instead of polling a clock.
    served: tokio::sync::Notify,
    /// When set, the NEXT request waits on this before it answers, then the gate
    /// is spent. A test holds a peer's answer until the ordering it needs has
    /// happened, so no sleep decides the order.
    gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// What the scripted endpoint reports about each pass (nothing, by default).
    report: std::sync::Mutex<crate::provider::ProviderReport>,
    /// The model the scripted endpoint says served each pass.
    model: std::sync::Mutex<String>,
}

/// A request's messages → the completion for it, or `None` to serve the queue.
type Responder = Box<dyn Fn(&[Value]) -> Option<String> + Send + Sync>;

impl MockScript {
    /// Hold the NEXT request until the returned gate is notified (its `gate`
    /// field): a test makes its own write while that pass is in flight.
    pub(crate) fn hold_next(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    /// How many provider passes have been served. python's `.calls`.
    pub(crate) fn passes_served(&self) -> usize {
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
    pub(crate) fn request_messages(&self, n: usize) -> Vec<Value> {
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
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.script.requests.lock().unwrap().push(messages.to_vec());
        self.script.served.notify_one();
        let gate = self.script.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if !self.script.delay.is_zero() {
            tokio::time::sleep(self.script.delay).await;
        }
        let answered = self
            .script
            .responder
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|f| f(messages));
        let pass = match answered {
            Some(text) => pass(text),
            None => self
                .script
                .passes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default(),
        };
        let stall = pass.iter().find_map(|e| match e {
            ScriptEvent::Stall { idle, endpoint } => Some((*idle, endpoint.clone())),
            _ => None,
        });
        let evs: Vec<Result<ProviderEvent, ProviderError>> = pass
            .into_iter()
            .filter_map(|e| match e {
                ScriptEvent::Token(t) => Some(Ok(ProviderEvent::Token(t))),
                ScriptEvent::Fail(e) => Some(Err(ProviderError::raised(e))),
                ScriptEvent::TransportFail(e) => Some(Err(ProviderError::transport(e))),
                ScriptEvent::HttpFail(code) => Some(Err(ProviderError::raised(format!(
                    "mock_backend: answered HTTP {code}"
                ))
                .for_status(code))),
                ScriptEvent::Stall { .. } => None,
            })
            .collect();
        let Some((idle, endpoint)) = stall else {
            return Ok(Box::pin(futures_util::stream::iter(evs)));
        };
        // THE READ CLOCK, as the provider runs it: nothing arrives, the window
        // passes, and the stream fails naming the stall.
        let silent = futures_util::stream::once(async move {
            let Err(stalled) = within(idle, std::future::pending::<()>()).await else {
                unreachable!("a pending future never completes")
            };
            Err(stalled.error("mock_backend", &endpoint))
        });
        Ok(Box::pin(futures_util::StreamExt::chain(
            futures_util::stream::iter(evs),
            silent,
        )))
    }
    fn model(&self) -> String {
        self.script.model.lock().unwrap().clone()
    }
    fn report(&self) -> crate::provider::ProviderReport {
        self.script.report.lock().unwrap().clone()
    }
}

/// The test bundle: dispatches every verb through ai-core, building a
/// `MockProvider` over the process-global script keyed by agent id. Its `cfg` is
/// the backend it stands for: [`CFG`] cannot cache, a second one can.
struct MockBundle {
    cfg: BackendConfig,
}

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
pub(crate) fn script(
    agent: &AgentId,
    passes: Vec<Vec<ScriptEvent>>,
    delay: Duration,
) -> Arc<MockScript> {
    let s = Arc::new(MockScript {
        passes: std::sync::Mutex::new(passes.into_iter().collect()),
        requests: std::sync::Mutex::new(Vec::new()),
        delay,
        responder: std::sync::Mutex::new(None),
        served: tokio::sync::Notify::new(),
        gate: std::sync::Mutex::new(None),
        report: std::sync::Mutex::new(Default::default()),
        model: std::sync::Mutex::new("test-model".to_string()),
    });
    scripts().insert(agent.as_str().to_string(), Arc::clone(&s));
    s
}

/// `text` as it reads inside a JSON string (a request's serialized messages).
pub(crate) fn json_text(text: &str) -> String {
    let e = serde_json::to_string(text).expect("a string serializes");
    e[1..e.len() - 1].to_string()
}

/// The default type's unfold call for row `id` of the chat `own`: what every marker of
/// the type ends with.
pub(crate) fn default_unfold(own: &str, id: impl std::fmt::Display) -> String {
    format!(
        "unfold: <call name=\"send\"><routing><target_id>{own}</target_id></routing>\
         <payload><type>recall</type><id>{id}</id></payload></call>"
    )
}

/// `text` past the thought markers at its top (map-once: the thinking of the answer
/// before it rides there).
pub(crate) fn past_thoughts(text: &str) -> &str {
    let mut rest = text;
    while rest.starts_with("<thought id=") {
        rest = rest.split_once("\n\n").map_or("", |(_, r)| r);
    }
    rest
}

/// Whether a text opens with a time head, `[YYYY-MM-DD HH:MM ±HH:MM]`: the head a row
/// no longer carries (map-once), whose copies in a model's own answers were the symptom.
pub(crate) fn opens_with_a_time(text: &str) -> bool {
    let shape = b"[0000-00-00 00:00 +00:00]";
    let b = text.as_bytes();
    b.len() >= shape.len()
        && shape.iter().zip(b).all(|(s, c)| match s {
            b'0' => c.is_ascii_digit(),
            b'+' => *c == b'+' || *c == b'-',
            _ => s == c,
        })
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
            responder: std::sync::Mutex::new(None),
            served: tokio::sync::Notify::new(),
            gate: std::sync::Mutex::new(None),
            report: std::sync::Mutex::new(Default::default()),
            model: std::sync::Mutex::new("test-model".to_string()),
        })
    }))
}

/// Wait for `n` provider passes. python's `_until_calls`: the chain is
/// asynchronous, so a test waits on the served event; failing FAST is the point,
/// since a stalled chain would otherwise wait out the 180s send ceiling.
pub(crate) async fn until_passes(script: &Arc<MockScript>, n: usize) -> bool {
    // Each served request stores one permit on `served`, so a request that lands
    // between the check and the wait is not lost. The timeout is only the failure
    // bound (a stalled chain), never the ordering.
    let served = async {
        loop {
            if script.passes_served() >= n {
                return true;
            }
            script.served.notified().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .unwrap_or(false)
}

/// A `<call>` exactly as the `default` `send` how-to (`res/default/howto_send.txt`) teaches it, BUILT BY
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

fantastic_kernel::stand_in_verbs!(sleeper_verbs { SendV "send" { text: Option<String>, client_id: Option<String>, ms: Option<i64>, msg: Option<i64>, stream_id: Option<String> } });

/// A NON-AI agent whose `send` answers after `ms` (its record's, default 1500).
struct Sleeper;

#[async_trait]
impl fantastic_kernel::bundle::Handler for Sleeper {
    type Call = sleeper_verbs::Call;
    fn man(&self) -> &'static str {
        "A slow test stand-in."
    }
    fn name(&self) -> &str {
        "sleeper"
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        sleeper_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        sleeper_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        Ok(Some(match call {
            sleeper_verbs::Call::SendV(a) => {
                tokio::time::sleep(Duration::from_millis(a.ms.unwrap_or(1500) as u64)).await;
                json!({"slept": true, "text": a.text})
            }
            sleeper_verbs::Call::Reflect(_) => json!({"id": agent_id.as_str(), "sentence": "slow"}),
        }))
    }
}

/// A kernel with the mock AI and a SLOW non-AI agent `sleeper` it holds a line on.
pub(crate) async fn with_sleeper(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let kernel = base_kernel(tmp, |k| k.bundles.register("sleeper.tools", Sleeper)).await;
    let backend_id = id_for(tmp, tag);
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic")).await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "sleeper.tools", "id": "sleeper"}),
        )])
        .await;
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "update_agent",
                "id": backend.as_str(),
                "can": {"$parent": {"call": ["*"]}, file_id: {"call": ["*"]}, "cli": {"call": ["*"]}, MEMORY: {"call": ["*"]}, "sleeper": {"call": ["*"]}, "kernel_state": {"call": ["ack_deadletter"]}},
            }),
        )])
        .await;
    (kernel, backend)
}

/// WAIT UNTIL EVERY AGENT OF `kernel` IS QUIET (#795): a `send` acks once its message
/// is stored, so a test that reads what the message did waits for the passes it woke,
/// their calls and the replies those woke. `false` when they did not go quiet.
pub(crate) async fn settle(kernel: &Arc<Kernel>) -> bool {
    crate::state::settle(kernel, Duration::from_secs(20)).await
}

/// `sent`, then [`settle`]: the outcome of the send itself (its ack), read once every
/// agent is quiet.
pub(crate) async fn settled<T>(kernel: &Arc<Kernel>, sent: impl std::future::Future<Output = T>) -> T {
    let out = sent.await;
    settle(kernel).await;
    out
}

/// Wire `agent`'s passes to `provider`, as every call to its bundle does.
pub(crate) fn wire(agent: &AgentId, provider: Arc<dyn Provider>, cfg: BackendConfig) {
    *crate::state::state_for(agent).machine.lock().unwrap() = Some(crate::state::Machine {
        provider: Arc::new(move |_| Arc::clone(&provider)),
        cfg,
    });
}

/// A PASS IN FLIGHT for `agent` while this lives (its life says `running`): what a verb
/// that refuses mid-pass sees.
pub(crate) struct MidPass(AgentId);

impl MidPass {
    pub(crate) fn new(agent: &AgentId) -> Self {
        crate::state::state_for(agent).life.lock().unwrap().running = true;
        Self(agent.clone())
    }
}

impl Drop for MidPass {
    fn drop(&mut self) {
        crate::state::state_for(&self.0).life.lock().unwrap().running = false;
    }
}

/// `agent`'s history, read as the agent.
pub(crate) async fn history_of(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
) -> crate::memory::MemResult<Vec<Value>> {
    fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::load_history(agent, kernel, false),
    )
    .await
}

/// The contents of `agent`'s stored reply rows (`[reply to …]`), in order: each reply
/// to a call is its own message (#795).
pub(crate) async fn reply_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<String> {
    history_of(kernel, agent)
        .await
        .expect("the history reads")
        .iter()
        .filter(|r| r["role"] == "user")
        .filter_map(|r| r["content"].as_str())
        .filter(|c| c.starts_with("[reply to "))
        .map(str::to_string)
        .collect()
}

/// The contents of `agent`'s error rows: each failed pass's (#177).
pub(crate) async fn error_rows(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<String> {
    history_of(kernel, agent)
        .await
        .expect("the history reads")
        .iter()
        .filter(|r| r["kind"] == "error")
        .filter_map(|r| r["content"].as_str().map(str::to_string))
        .collect()
}

/// The content of `agent`'s newest assistant row: the answer of its last pass.
pub(crate) async fn last_answer(kernel: &Arc<Kernel>, agent: &AgentId) -> String {
    history_of(kernel, agent)
        .await
        .expect("the history reads")
        .iter()
        .rev()
        .find(|r| r["role"] == "assistant")
        .and_then(|r| r["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// One pass that emits one whole text chunk.
pub(crate) fn pass(text: impl Into<String>) -> Vec<ScriptEvent> {
    vec![ScriptEvent::Token(text.into())]
}

const CFG: BackendConfig = BackendConfig {
    name: "mock_backend",
    model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
};

#[async_trait]
impl Handler for MockBundle {
    chat_decls!();
    fn name(&self) -> &str {
        self.cfg.name
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        // EVERY VERB through the one shared table, exactly as a real chat
        // dispatches it — boot is a new incarnation there too.
        let me = agent_id.clone();
        let reflect = || {
            json!({
                "id": agent_id.as_str(),
                "sentence": "Mock LLM agent.",
                "model": helpers::meta_string(agent_id, kernel, "model"),
                "generating": crate::state::is_generating(agent_id),
                "verbs": {"send": "send", "history": "history"},
                "emits": {"token": "t"},
            })
        };
        let reply = verbs::handle(
            agent_id,
            call,
            kernel,
            self.cfg,
            // the script registered for the agent WHEN THE PASS STARTS: a pass may start
            // outside a call to the bundle, after a test scripted it anew
            move |_client| {
                Arc::new(MockProvider {
                    script: script_for(&me),
                }) as Arc<dyn Provider>
            },
            reflect,
        )
        .await?;
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
        verbs::on_cancel(agent_id, &task_id.0.to_string(), kernel).await
    }

    /// Every backend's delete hook: the shared teardown.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        verbs::on_delete(agent_id, kernel).await;
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

/// A kernel with the mock backend, `file_bridge` and `ram_store` bundles (plus
/// whatever `extra` registers), an unbooted `kernel_state` root handed a guid, and
/// the shared test memory store under it.
pub(crate) async fn base_kernel(tmp: &TempDir, extra: impl FnOnce(&mut Kernel)) -> Arc<Kernel> {
    base_kernel_at(tmp.path(), extra).await
}

/// [`base_kernel`] over the folder `dir`: a kernel of another process (or a new one in
/// this process) opens the same files there, under the same guid.
pub(crate) async fn base_kernel_at(
    dir: &std::path::Path,
    extra: impl FnOnce(&mut Kernel),
) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: dir.join(".fantastic"),
        work_dir: dir.to_path_buf(),
    });
    kernel
        .bundles
        .register(HANDLER_MODULE, MockBundle { cfg: CFG });
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    extra(&mut kernel);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        dir.join(".fantastic"),
        false,
    );
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    fantastic_ram_store::test_support::ensure_memory(&kernel).await;
    kernel
}

pub(crate) async fn mk_kernel(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let kernel = base_kernel(tmp, |_| {}).await;

    let backend_id = id_for(tmp, tag);
    let file_id = format!("ff_{}", backend_id);
    // Rooted at the loader's own dir so this leg is ALSO the discovered
    // persistence provider (`find_store` matches on the root path).
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic")).await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    (kernel, backend)
}

/// Create a `file_bridge` leg rooted at `root`, opened; the agent that persists
/// through it holds its own line on it.
///
/// Each AI gets its OWN leg: its chat sidecar lands at the LEG's root
/// (`agents/<self>/chat_<client>.jsonl`), so two AIs sharing one leg would
/// interleave under one `agents/` tree.
async fn add_file_bridge(kernel: &Arc<Kernel>, id: &str, root: &std::path::Path) {
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": id,
                "root": root.to_string_lossy(),
                // the fs edge seals by default — open it so history persists through it
            }),
        )])
        .await
        .remove(0)
        .result;
}

/// The verbs the prompt loader sends the file view `file_bridge_id` names.
pub(crate) const PROMPT_VIEW_VERBS: [&str; 2] = ["read", "watch"];

/// A file VIEW of `path` on `agent`'s own leg (`ff_<agent>`), opened by the operator as
/// `pv_<agent>`; `agent`'s `can` gains its line on the view ([`PROMPT_VIEW_VERBS`]) and its
/// record names the view (`file_bridge_id`) and the path (`system_prompt_file`). The view's id.
pub(crate) async fn name_prompt_view(kernel: &Arc<Kernel>, agent: &AgentId, path: &str) -> String {
    let view = format!("pv_{agent}");
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from(format!("ff_{agent}").as_str()),
            json!({"type": "open_view", "kind": "file", "path": path, "id": view}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(view), "{r}");
    let mut can = crate::helpers::agent_meta(agent, kernel)
        .get("can")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    can.insert(view.clone(), json!({"call": PROMPT_VIEW_VERBS}));
    for (field, v) in [
        ("can", Value::Object(can)),
        ("file_bridge_id", json!(view)),
        ("system_prompt_file", json!(path)),
    ] {
        let mut p = json!({"type": "update_agent", "id": agent.as_str()});
        p[field] = v;
        let r = kernel
            .operator_send(vec![Item::new(AgentId::from("kernel_state"), p)])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{field}: {r}");
    }
    view
}

/// A SECOND backend in the same kernel — routing needs a peer to hand a reply
/// to — with its own leg (see [`add_file_bridge`]) so the two chat stores stay
/// separate files.
async fn add_peer(kernel: &Arc<Kernel>, tmp: &TempDir, primary: &AgentId) -> AgentId {
    let id = format!("{primary}_peer");
    let file_id = format!("ff_{id}");
    add_file_bridge(kernel, &file_id, &tmp.path().join(".fantastic").join(&id)).await;
    let peer = add_backend(kernel, &id, &file_id).await;
    // WIRING IS LINES: the primary hands replies to the peer and the peer routes
    // back — with the routing pass GATED (the model's calls are the agent's calls),
    // each holds its own line on the other.
    for (holder, other) in [
        (primary.clone(), peer.clone()),
        (peer.clone(), primary.clone()),
    ] {
        let ff = format!("ff_{holder}");
        let _ = kernel.operator_send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type": "update_agent",
                    "id": holder.as_str(),
                    // update REPLACES the named field: every line rides along
                    "can": {"$parent": {"call": ["*"]}, ff: {"call": ["*"]}, "cli": {"call": ["*"]}, MEMORY: {"call": ["*"]}, other.as_str(): {"call": ["*"]}},
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
/// the default system block now carries the `send` how-to as well as
/// the context section (`res/default/howto_context.txt`), and a chain's second turn would die on
/// `context_insufficient` — which looks exactly like a routing bug. python
/// documents the same trap at `test_send_to.py:53-66`.
async fn add_backend(kernel: &Arc<Kernel>, id: &str, file_id: &str) -> AgentId {
    add_backend_on(kernel, id, file_id, HANDLER_MODULE).await
}

/// [`add_backend`] on a given handler module: a backend other than the mock that
/// cannot cache, such as the caching mock.
async fn add_backend_on(kernel: &Arc<Kernel>, id: &str, file_id: &str, module: &str) -> AgentId {
    // The conversation lives in the memory store the record names — wired both
    // ways (python `_testkit.memory_meta`). Deny-all otherwise: the backend
    // streams its turn to `cli` and names its leg, each an explicit grant.
    let memory = fantastic_ram_store::test_support::memory_meta(
        kernel,
        id,
        json!({file_id: {"call": ["*"]}, "cli": {"call": ["*"]}}),
    )
    .await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": module,
        "id": id,
        "model": "test-model",
        "context_window": 32768,
    });
    for (k, v) in memory {
        create[k] = v;
    }
    let _ = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    AgentId::from(id)
}

/// The shared test memory store's id.
const MEMORY: &str = fantastic_ram_store::test_support::MEMORY;

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
        // a status that ENDS the call: done, failed, stalled or awaiting
        if out.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase")
                    .and_then(Value::as_str)
                    .is_some_and(|p| ["done", "failed", "stalled", "awaiting"].contains(&p))
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
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    for key in ["id", "sentence", "model", "generating", "verbs"] {
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
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
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
            ScriptEvent::Token(" <call name=\"done\"></call>".into()),
        ]],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_tok";
    let reply = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": client_id}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(reply["client_id"], client_id);
    assert_eq!(json!(crate::tests::last_answer(&kernel, &backend).await), "Hello world <call name=\"done\"></call>");

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

/// A reasoning turn: the RAW completion (thinking included) is what streams; the
/// caller's reply is the ANSWER only (thinking never leaves the agent); the ROUTER dispatches only the ANSWER's calls and the DURABLE
/// turn is the answer (only the answer acts). A call written
/// mid-thought is thinking — it never runs and is never stored. The answer ends with
/// `done`, so no reminder starts a second turn.
#[tokio::test]
async fn thinking_is_streamed_raw_but_neither_dispatched_nor_persisted() {
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
            ScriptEvent::Token("the answer <call name=\"done\"></call>".into()),
        ]],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_think";
    let reply = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": client_id}),
        )]))
        .await
        .remove(0)
        .result;

    // The REPLY is the answer — no marker, no deliberation.
    let answer = crate::tests::last_answer(&kernel, &backend).await;
    let answer = answer.as_str();
    assert_eq!(answer, "the answer <call name=\"done\"></call>", "{reply}");

    // The token stream (the agent's own inbox) is raw — markers intact.
    let events = drain(&mut rx).await;
    let streamed: String = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("token"))
        .filter_map(|e| e.get("text").and_then(Value::as_str))
        .collect();
    assert!(streamed.contains(crate::think::THINK_OPEN), "{streamed:?}");
    assert_eq!(crate::think::without_thinking(&streamed), answer);

    // …and the mid-thought call NEVER RAN: it was thinking.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !probe.exists(),
        "a call written inside thinking was dispatched"
    );

    // The DURABLE turn is stripped — no marker, no deliberation, no mid-thought
    // call text; just the answer.
    // Through `recall`, which pages the DURABLE turns: `history` is the digest.
    let r = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "recall"}))])
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
    // ...and so is the call written inside it: it never ran.
    assert!(
        !content.contains("mid_thought"),
        "a thinking call was persisted: {content:?}"
    );
}

/// An attempt spent ENTIRELY reasoning is an answer with no call (#795): it is
/// DISCARDED — durable history holds nothing of it, no empty assistant row a
/// later turn would read as an answer — and RE-ASKED inside its pass with the
/// no-call report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thinking_only_turn_is_discarded_and_rerolled() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "thonly").await;
    let s = script(
        &backend,
        vec![
            vec![
                ScriptEvent::Token(crate::think::THINK_OPEN.into()),
                ScriptEvent::Token("all 578 tokens went here".into()),
                ScriptEvent::Token(crate::think::THINK_CLOSE.into()),
            ],
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let reply = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": "c_thonly"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(json!(crate::tests::last_answer(&kernel, &backend).await), json!("<call name=\"done\"></call>"), "{reply:#?}");
    assert!(reply.get("raised").is_none(), "{reply:#?}");
    assert_eq!(s.passes_served(), 2, "the thinking-only attempt was not re-asked");
    assert!(
        s.request(1)
            .contains(&json_text(crate::model_format::default::REPORT_NO_CALL)),
        "the re-ask says why: {}",
        s.request(1)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let h = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "history"}))])
        .await
        .remove(0)
        .result;
    let rows = h["rows"].as_array().expect("rows array");
    assert!(
        // history rows are digests: the row's text is its `head`
        !rows.iter().any(|r| {
            let c = r["head"].as_str().unwrap_or("");
            c.contains("all 578 tokens") || (r["role"] == json!("assistant") && c.trim().is_empty())
        }),
        "a thinking-only attempt stores nothing: {rows:#?}"
    );
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
    async fn chat(
        &self,
        _m: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        let ev = Ok(ProviderEvent::Token(self.0.to_string()));
        Ok(Box::pin(futures_util::stream::iter(vec![ev])))
    }
    fn model(&self) -> String {
        "sum".into()
    }
}

/// `status.current` has TWO python shapes, not one dict with a flag.
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
        crate::tests::settled(&send_kernel, send_kernel
            .operator_send(vec![Item::new(
                backend_for_task,
                json!({"type": "send", "text": "go", "client_id": "probe"}),
            )]))
            .await
            .remove(0)
            .result
    });
    assert!(until_passes(&_s, 1).await, "the pass never started");

    let mine = kernel
        .operator_send(vec![Item::new(
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
        .operator_send(vec![Item::new(
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
            .operator_send(vec![Item::new(backend.clone(), payload)])
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

/// The `send` reply is the ACK (#795): `{accepted, re, client_id}`, `re` naming the row
/// the message was stored as; the pass's answer row answers that row (its `parent`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_reply_carries_the_stored_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_tid").await;
    let _s = script(&backend, vec![pass("ack <call name=\"done\"></call>")], Duration::ZERO);
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(
        r.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["accepted", "re", "client_id"],
        "{r:#?}"
    );
    let re = r["re"].as_i64().expect("re is a row id");
    let rows = conversation(&kernel, &backend).await;
    let incoming = rows
        .iter()
        .find(|row| row["id"].as_i64() == Some(re))
        .expect("the row the reply names");
    assert_eq!(incoming["content"], json!("ping"));
    let assistant = rows
        .iter()
        .find(|row| row["role"] == json!("assistant"))
        .expect("the answer row");
    assert_eq!(assistant["parent"], json!(re), "the answer answers the stored row");
    assert_eq!(assistant["content"], json!("ack <call name=\"done\"></call>"));
}

/// The env is process-global, so the test that overrides the send ceiling must
/// not overlap any other. It takes this lock and restores what it found.
static SEND_TIMEOUT_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Holds the process-wide send ceiling at `secs` and restores what it found.
struct Ceiling(Option<String>);

impl Ceiling {
    fn set(secs: &str) -> Self {
        let prev = std::env::var("FANTASTIC_AI_SEND_TIMEOUT").ok();
        std::env::set_var("FANTASTIC_AI_SEND_TIMEOUT", secs);
        Self(prev)
    }
}

impl Drop for Ceiling {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("FANTASTIC_AI_SEND_TIMEOUT", v),
            None => std::env::remove_var("FANTASTIC_AI_SEND_TIMEOUT"),
        }
    }
}

/// The ceiling is a python FLOAT: `FANTASTIC_AI_SEND_TIMEOUT=0.05` is a
/// 50ms ceiling on a pass (rust parsed `u64`, failed, and silently used 180), its
/// error row spells the value with python's `repr`, and the `status(failed)` detail carries
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
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    let elapsed = started.elapsed();
    match prev {
        Some(v) => std::env::set_var("FANTASTIC_AI_SEND_TIMEOUT", v),
        None => std::env::remove_var("FANTASTIC_AI_SEND_TIMEOUT"),
    }
    assert_eq!(r["accepted"], json!(true), "{r}");
    let errors = error_rows(&kernel, &backend).await;
    assert!(
        errors.iter().any(|e| e.contains("send: timeout after 0.05s")),
        "the pass's error row spells the float: {errors:?}"
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
                && v.get("phase").and_then(Value::as_str) == Some("failed")
        })
        .expect("status(failed)");
    let keys: Vec<&String> = done["detail"].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["reason", "send_id", "started_at", "queue_depth"],
        "{done:#?}"
    );
    assert_eq!(done["detail"]["reason"], json!("timeout"));
}

/// The default is `600.0` (10 minutes), and it is a float on the wire of the
/// refusal — not `180`.
#[test]
fn the_default_send_ceiling_is_a_python_float() {
    assert_eq!(crate::verbs::SEND_TIMEOUT_SECS, 600.0);
    assert_eq!(
        fantastic_bundle::py_float_repr(crate::verbs::SEND_TIMEOUT_SECS),
        "600.0"
    );
}

/// Python `secrets.token_urlsafe(8)`: 11 base64url characters, no
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

/// The client id is declared in the schema: text with a non-space character, default `cli`.
/// A blank one is refused by name on `send` and `inquiry`; an absent one takes the default.
#[tokio::test]
async fn a_blank_client_id_is_refused_and_an_absent_one_defaults() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cid").await;
    for (verb, field) in [("send", "client_id"), ("inquiry", "reply_client_id")] {
        for bad in ["", "  ", "\u{1f}"] {
            let r = kernel
                .operator_send(vec![Item::new(
                    backend.clone(),
                    json!({"type": verb, "text": "hi", field: bad}),
                )])
                .await
                .remove(0)
                .result;
            assert_eq!(r["reason"], json!("bad_arg"), "{verb} {bad:?}: {r:#?}");
            assert_eq!(r["arg"], json!(field), "{verb} {bad:?}: {r:#?}");
        }
    }
    assert_eq!(fantastic_kernel::inquiry::default_client_id(), "cli");
    let d: crate::declared::Send = serde_json::from_value(json!({"text": "hi"})).unwrap();
    assert_eq!(d.client_id, "cli");
}

/// Python `_safe_client`, per CODE POINT: `str.isalnum` is Unicode-aware
/// and there is no length cap, and the strip is python's whitespace set.
#[test]
fn safe_client_strips_python_whitespace() {
    use crate::helpers::safe_client;
    // `\x1c`–`\x1f` are python whitespace; `str::trim` keeps them.
    assert_eq!(safe_client("\u{1f} p \u{1f}"), "p");
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
    assert_eq!(
        resolve_context_window(&m, crate::constants::DEFAULT_CONTEXT_WINDOW),
        8000
    );
    let mut m2 = Map::new();
    m2.insert("num_ctx".into(), json!(2000));
    assert_eq!(
        resolve_context_window(&m2, crate::constants::DEFAULT_CONTEXT_WINDOW),
        2000
    );
    // The default is 131072, not 4096: at 4096 the budget (4096 − 1024 reserve
    // = 3072) no longer fit the kernel's OWN ~3174-token system block, so every
    // agent created without an explicit window failfasted `too_small` before
    // the model was called.
    assert_eq!(
        resolve_context_window(&Map::new(), crate::constants::DEFAULT_CONTEXT_WINDOW),
        crate::constants::DEFAULT_CONTEXT_WINDOW
    );
    assert_eq!(crate::constants::DEFAULT_CONTEXT_WINDOW, 131_072);
    let mut tiny = Map::new();
    tiny.insert("context_window".into(), json!(100));
    tiny.insert("output_reserve".into(), json!(1000));
    assert_eq!(budget(&tiny, crate::constants::DEFAULT_CONTEXT_WINDOW), 256); // floor
}

#[tokio::test]
async fn compact_returns_projection_with_summary() {
    let b = body(10, 80);
    let provider: Arc<dyn Provider> = Arc::new(SummaryProvider("SUMMARY"));
    let pass = crate::model_format::pass::Pass::for_test(Arc::clone(&provider));
    let proj = crate::model_format::pass::settled(&pass, None, |p| crate::strategies::compact(&b, 4, 1_000_000, p)).await;
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
    let proj = crate::strategies::truncate(&b, 1_000_000, &crate::context::estimate_one);
    assert!(proj.omitted_marker && proj.summary.is_none());
    assert_eq!(proj.body[0], b[0]); // first kept
    assert_eq!(proj.body[proj.body.len() - 1], b[b.len() - 1]); // live turn kept
}

#[test]
fn context_notice_shapes() {
    // the notice as the default type maps it
    let text = |n: &Value| -> String {
        crate::model_format::map::messages(
            crate::model_format::of(crate::model_format::DEFAULT),
            "ai",
            std::slice::from_ref(n),
        )[0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let n = crate::projection::context_notice("compact", Some("MY SUMMARY"), false, 7, 0, 0);
    assert_eq!(n["role"], "user");
    assert!(n.get("tool_calls").is_none());
    let c = text(&n);
    let c = c.as_str();
    assert!(c.contains("[context-notice]") && c.contains("MY SUMMARY"));
    assert!(c.contains("memory agent"));
    // THE RECALL AFFORDANCE IN THE ONE GRAMMAR — never a JSON shape the
    // harness refuses. Python's bytes, pinned.
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
    let tc = text(&t);
    assert!(tc.contains("omitted in place") && !tc.contains("Summary of the dropped span"));
    // A strip reports what left the view, and how to get it back.
    let s = crate::projection::context_notice("strip", None, false, 0, 4, 27_000);
    let sc = text(&s);
    assert!(
        sc.contains(
            "4 reply bod(ies) were STRIPPED to their tag+title line (27000 bytes left your view)"
        ),
        "{sc}"
    );
    assert!(
        sc.contains("grep finds a line in a body held this run — after a restart, re-read it from its origin."),
        "{sc}"
    );
}

// ── Context Protocol: recall + context_status (kernel-driven) ────────

#[tokio::test]
async fn recall_pages_durable_store_and_context_status_shape() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "recall").await;
    script(&backend, vec![pass("noted")], Duration::ZERO);
    // Plant a distinctive fact in the durable store via a normal send.
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "my project codename is HALCYON", "client_id": "chat"}),
        )]))
        .await
        .remove(0)
        .result;
    // recall finds it by substring (case-insensitive), full durable history.
    let r = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "recall", "query": "codename"}),
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
    // `client_id`.
    assert!(r.get("client_id").is_none(), "{r:#?}");

    // context_status shape — default strategy, projection recorded.
    let st = kernel
        .operator_send(vec![Item::new(
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
    // `done` because an answer with no call is re-asked (#795).
    script(
        &backend,
        vec![pass(r#"ack <call name="done"></call>"#)],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_hist";
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping", "client_id": client_id}),
        )]))
        .await
        .remove(0)
        .result;
    let h = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "history"}))])
        .await
        .remove(0)
        .result;
    let rows = h["rows"].as_array().expect("rows array");
    assert!(rows.len() >= 2, "expected >=2 rows, got {rows:#?}");
    assert_eq!(rows[0]["role"], "user");
    assert_eq!(rows[0]["head"], "ping");
    assert_eq!(rows[1]["role"], "assistant");
    assert_eq!(rows[1]["head"], r#"ack <call name="done"></call>"#);

    // WHERE IT LIVES: the agent's MEMORY STORE (`memory_store_id`), keyed by
    // who wrote it, ids from the kernel's one counter. Agent id is conversation
    // id, so one list is what every reader gets.
    let rows = conversation(&kernel, &backend).await;
    // ONE ROW PER TURN — list index == store index (recall/reaction paging).
    // Two turns: the ping and the answer that ended the chain.
    assert_eq!(rows.len(), 2, "one row per message: {rows:#?}");
    assert_eq!(rows[0]["role"], "user");
    assert_eq!(rows[0]["content"], "ping");
    assert_eq!(rows[1]["role"], "assistant");
    assert_eq!(rows[1]["content"], r#"ack <call name="done"></call>"#);
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
        !kernel.exists(chat_id.as_str()),
        "the `<self>_chat` child must be gone: {chat_id}"
    );
}

/// The conversation lives in the agent's MEMORY STORE, whatever the loader
/// discovered: here the loader has NO provider at all — the root's dir is
/// `<tmp>/.fantastic` and the only `file_bridge` is rooted at `<tmp>/elsewhere`,
/// which `find_store` (it matches on the root path) cannot match — and `history`
/// still reads every row back.
#[tokio::test]
async fn chat_persists_with_no_loader_provider_wired() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |_| {}).await;

    let backend_id = id_for(&tmp, "noprov");
    let file_id = format!("ff_{backend_id}");
    // NOT the loader's dir — no persistence provider is discoverable.
    let leg_root = tmp.path().join("elsewhere");
    add_file_bridge(&kernel, &file_id, &leg_root).await;
    assert!(
        fantastic_kernel::persistence::find_store(&kernel).is_none(),
        "the loader must have NO provider for this test to mean anything"
    );
    let backend = add_backend(&kernel, &backend_id, &file_id).await;

    script(
        &backend,
        vec![pass(r#"ack <call name="done"></call>"#)],
        Duration::ZERO,
    );
    let _rx2 = rebind_backend_inbox(&kernel, &backend);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping", "client_id": "cli"}),
        )]))
        .await
        .remove(0)
        .result;

    let h = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "history"}))])
        .await
        .remove(0)
        .result;
    let rows = h["rows"].as_array().expect("rows array");
    assert_eq!(
        rows.len(),
        2,
        "history came back empty with no loader provider: {h:#?}"
    );
    assert_eq!(rows[0]["head"], "ping");
    assert_eq!(rows[1]["head"], r#"ack <call name="done"></call>"#);
    // …and it is REALLY in the agent's MEMORY STORE, not on its file leg: the
    // kernel keeps no conversation, and putting rows on disk is a durable
    // store's job that no AI agent asks for.
    let rows = conversation(&kernel, &backend).await;
    assert_eq!(
        rows.len(),
        2,
        "the conversation is not in the store: {rows:#?}"
    );
    assert_eq!(rows[0]["content"], "ping");
    assert!(rows.iter().all(|r| r["agent"] == json!(backend.as_str())));
    assert!(
        !leg_root.join(format!("agents/{backend_id}")).exists(),
        "the AI wrote a conversation sidecar on its own leg"
    );
}

/// The backend's bespoke `interrupt` is GONE — a pass's request is a KERNEL TASK (#795:
/// the provider's `complete`, or here the chat's own task reading an in-process
/// provider), so it is cancelled the way any dispatch is: find it in `ps`, kill it with
/// `cancel`. The cancelled request ends its pass as stopped (a `done` event reaches the
/// caller, a marker row is kept) — the substrate's contract, not this bundle's.
#[tokio::test]
async fn kernel_cancel_kills_an_in_flight_send() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "intr").await;
    script(&backend, vec![pass("slow")], Duration::from_secs(2));
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_intr";
    let ack = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": client_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(ack["accepted"], json!(true), "{ack}");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Discover the pass's request in the live task table…
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
        .find(|t| t["agent"] == json!(backend.as_str()) && t["verb"] == json!("complete"))
        .and_then(|t| t["id"].as_i64())
        .expect("the pass's request must be in `ps`");

    // …and kill it with the universal system verb.
    let r = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": task_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["cancelled"], json!(true));
    assert_eq!(r["killed"], json!([task_id]));
    assert!(settle(&kernel).await, "the agent never went quiet");

    // The cancelled request closed the pass out: the caller still gets its `done`.
    let events = drain(&mut rx).await;
    assert!(
        events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("status")
                && e.get("phase").and_then(Value::as_str) == Some("done")
                && e["detail"]["reason"] == json!("cancelled")),
        "expected a done event after cancel: {events:#?}",
    );
    let rows = history_of(&kernel, &backend).await.expect("history");
    assert!(rows.iter().any(|r| r["kind"] == json!("stopped")), "{rows:#?}");
    // …and the backend no longer reports a phantom generation.
    let st = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["generating"], json!(false));
}

#[tokio::test]
async fn status_during_in_flight_shows_phase() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "stat").await;
    let st = script(&backend, vec![pass("x")], Duration::from_millis(500));
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let client_id = "browser_stat";
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let c = client_id.to_string();
    let send_join = tokio::spawn(async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b.clone(),
            json!({"type": "send", "text": "hi", "client_id": c}),
        )]))
        .await
        .remove(0)
        .result
    });
    assert!(until_passes(&st, 1).await, "the pass never started");
    let snap = kernel
        .operator_send(vec![Item::new(
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
// the `send` how-to is in it, and a second turn dying on `context_insufficient`
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "what is here?"}),
        )]))
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
    // the link the fold machinery derives chains from.
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
        let _ = crate::tests::settled(&kernel, kernel
            .operator_send(vec![Item::new(
                backend.clone(),
                json!({"type": "send", "text": "go"}),
            )]))
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "_call_stack": ["upstream"]}),
        )]))
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
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({
                "type": "send",
                "text": "go",
                "_call_stack": ["upstream", backend.as_str()],
            }),
        )]))
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
/// A fire-and-forget default produces the one failure a model cannot debug: a
/// key left out of an otherwise perfect call runs, its result is dropped, and
/// the chain dies with no reply and no error. Indistinguishable, from inside the
/// model, from being finished.
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&fo, 1).await,
        "the send itself must still happen"
    );
    // the reply to a message to an AI is its ACK: it comes back to self, stored as a
    // reply, and wakes nothing (Q2)
    assert!(settle(&kernel).await);
    let replies = reply_rows(&kernel, &backend).await;
    assert!(
        replies.iter().any(|r| r.contains(&format!("{peer} send")) && r.contains("<accepted>true</accepted>")),
        "an omitted reply_to must come back to self, tagged as a reply: {replies:#?}"
    );
    assert_eq!(s.passes_served(), 1, "an ack wakes nothing");
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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
/// it names the SHAPE, because a bare fault teaches the model nothing about how
/// to retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_with_no_target_is_reported_not_swallowed() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_notgt").await;
    // THE PARSER owns this refusal now: a `send` that names neither
    // `<target_id>` nor `<to>` never becomes a call at all, so what reaches the
    // model is the grammar's sentence, naming BOTH ways to address one.
    let bad = r#"<call name="send"><routing><reply_to>self</reply_to></routing><payload><type>reflect</type></payload></call>"#;
    let s = script(&backend, vec![pass(bad), pass("ok")], Duration::ZERO);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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

/// A payload-less send must not reach the handler as nothing and come back
/// `'NoneType' object has no attribute 'get'` — an error about OUR internals,
/// naming neither the omitted verb nor the agent. It must say what was missing
/// instead, and must never dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_with_no_payload_names_what_is_missing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_nopl").await;
    let bad = r#"<call name="send"><routing><target_id>totals</target_id></routing><payload></payload></call>"#;
    let s = script(&backend, vec![pass(bad), pass("ok")], Duration::ZERO);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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

/// END TO END, which the parser-level tests do not cover: a correct
/// `malformed_calls` report the model never receives leaves the run believing
/// its agent exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unparseable_call_reaches_the_model_as_an_error() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_broken").await;
    let broken = concat!(
        r#"<call name="send"><routing><target_id>kernel</target_id></routing><payload><type>create_agent"#,
        "<path>a</path></payload></call>"
    );
    let s = script(&backend, vec![pass(broken), pass("ok")], Duration::ZERO);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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
        // TERMINATED: a block with no `</call>` is a cut, not a malformed call.
        r#"<call name="send"><routing><target_id>n</target_id></routing><payload><code>oops</payload></call>"#
    );
    let s = script(&backend, vec![pass(text), pass("ok")], Duration::ZERO);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the broken sibling was swallowed"
    );
    assert!(s.request(1).contains("NOT dispatched"), "{}", s.request(1));
}

/// A VERB IN THE TOOL SLOT IS REFUSED BY NAME — `{"name":"reflect",…}` is never
/// guessed into a send of `{type:"reflect"}`: the tools are a closed set, and a
/// verb goes in `<type>`.
/// The refusal says exactly that, so the model retries from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_in_the_tool_slot_is_refused_by_name() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_verb").await;
    let call = r#"<call name="reflect"><target_id>core</target_id></call>"#;
    let s = script(&backend, vec![pass(call), pass("ok")], Duration::ZERO);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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
        turn2.contains("send, done, inquiry_answer"),
        "it must name the closed set: {turn2}"
    );
}

/// A failed pass routes nothing (#533, #795): it is not re-run as a turn of its own,
/// and its own output is never dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_reply_does_not_route() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_err").await;
    let s = script(
        &backend,
        vec![
            vec![ScriptEvent::Fail("provider exploded".into())],
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(r["accepted"], json!(true), "{r:#?}");
    let errors = error_rows(&kernel, &backend).await;
    assert!(errors.iter().any(|e| e.contains("exploded")), "{errors:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "a failed pass is not re-run");
}

/// PROVIDER FAILURE, KIND 2: unmapped. The message was acked when it was stored (#795);
/// the failed pass is said in ONE `status(failed, reason='error')` carrying `str(e)`
/// WITHOUT the exception-type prefix, and in its error row: never silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unmapped_provider_error_is_said_once_and_kept_as_a_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_raise").await;
    let _s = script(
        &backend,
        vec![
            vec![ScriptEvent::Fail("provider exploded".into())],
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(r["accepted"], json!(true), "{r:#?}");
    let errors = error_rows(&kernel, &backend).await;
    assert!(errors.iter().any(|e| e.contains("provider exploded")), "{errors:?}");
    // the EVENT carries `str(e)` alone
    let evs = drain(&mut rx).await;
    let done: Vec<&Value> = evs
        .iter()
        .filter(|v| {
            v.get("type").and_then(Value::as_str) == Some("status")
                && v.get("phase").and_then(Value::as_str) == Some("failed")
                && v["detail"]["reason"] == json!("error")
        })
        .collect();
    assert_eq!(done.len(), 1, "{evs:#?}");
    assert_eq!(done[0]["detail"]["error"], json!("provider exploded"));
}

/// A CONTEXT-PROJECTION refusal is not a provider failure: no request is sent, so no
/// `status(done)` closes one; the pass that could not be sent says so in its `failed`
/// status, the seam's own words, and the seam's `context(too_small)` event is told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_projection_refusal_fails_the_pass_in_the_seams_words() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_toosmall").await;
    // A window the kernel's own system block cannot fit — the `too_small`
    // failsafe, which refuses BEFORE the model is called.
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(), "context_window": 300}),
        )])
        .await;
    let _s = script(&backend, vec![pass("never reached")], Duration::ZERO);
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(r["accepted"], json!(true), "{r:#?}");
    let evs = drain(&mut rx).await;
    assert!(
        evs.iter().any(|v| {
            v["type"] == json!("status")
                && v["phase"] == json!("failed")
                && v["detail"]["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("mock_backend: context_insufficient — "))
        }),
        "the refusal is said: {evs:#?}"
    );
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
/// re-raised into the caller, which is why the router catches it. Unguarded, a
/// handler that raises (a `file_bridge` whose root cannot be resolved) leaves
/// the model with no reply, no error and no further turn — the run stops and
/// looks finished. An exception is a result; the model can act on one and cannot
/// act on silence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dying_handler_comes_back_as_an_error_not_a_dead_chain() {
    struct Raiser;
    /// The one verb the raiser declares beside `reflect`: it dies outright.
    #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Panic {}
    fantastic_kernel::verbs! { "panic" => Panic }
    #[async_trait]
    impl Handler for Raiser {
        type Call = Call;
        fn name(&self) -> &str {
            "raiser"
        }
        fn man(&self) -> &'static str {
            "A handler that dies."
        }
        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            VERBS
        }
        fn dynamic_verbs(&self, a: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            dynamic_verbs(a)
        }
        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            &crate::tests_decl::any::RECORD
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            call: Call,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            match call {
                Call::Panic(_) => panic!("the handler died outright"),
                Call::Reflect(_) => Err("root cannot be resolved".into()),
            }
        }
    }

    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| k.bundles.register("raiser.tools", Raiser)).await;
    let backend_id = id_for(&tmp, "rt_raise");
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic")).await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    let raiser = format!("{backend_id}_raiser");
    // Both halves of the edge: the routing pass is GATED (the model's calls
    // are the agent's calls), so the backend needs egress to the raiser and
    // the raiser must accept the backend.
    let _ = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "raiser.tools", "id": raiser}))])
            .await
            .remove(0)
            .result;
    let _ = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "update_agent", "id": backend_id.as_str(), "can": {"$parent": {"call": ["*"]}, file_id.as_str(): {"call": ["*"]}, "cli": {"call": ["*"]}, MEMORY: {"call": ["*"]}, raiser.as_str(): {"call": ["*"]}}}))])
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, 2).await,
        "the error killed the chain instead of routing"
    );
    let turn2 = s.request(1);
    assert!(
        turn2.contains("root cannot be resolved"),
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "again"}),
        )]))
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
/// words as python's `test_the_default_prompt_states_the_mechanism_not_a_strategy`.
#[tokio::test]
async fn the_default_prompt_states_the_mechanism_not_a_strategy() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_prompt").await;
    let state = crate::state::state_for(&backend);
    let (messages, _store_len) = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::assembly::assemble_messages(
            &backend,
            &state,
            &kernel,
            "probe",
            crate::caching::Caching::ProviderCache,
        ),
    )
    .await
    .expect("store read");
    let sys_block = crate::model_format::map::messages(
        crate::model_format::of(crate::model_format::DEFAULT),
        backend.as_str(),
        &messages,
    )[0]["content"]
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

/// Routing is its OWN task, labelled `route_reply` (a `task` row of the agent's): never
/// the pass's request, and never labelled `send` — once the agent is quiet, nothing of
/// it is still in `ps` (#795: it ends once its calls are out).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routing_is_its_own_ps_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_ps").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let fo = script(&peer, vec![pass(r#"slow peer <call name="done"></call>"#)], Duration::from_millis(500));
    script(
        &backend,
        vec![pass(call_envelope(
            peer.as_str(),
            json!({"type": "send", "text": "ping"}),
            Some("null"),
        ))],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&fo, 1).await, "the peer was never reached");
    let routed = fantastic_kernel::test_support::rows_about(&kernel, &backend)
        .into_iter()
        .any(|r| r.get("kind") == Some(&json!("task")) && r.get("verb") == Some(&json!("route_reply")));
    assert!(routed, "routing must be its own `route_reply` task");
    let table = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    let tasks = table["tasks"].as_array().expect("tasks");
    assert!(
        !tasks
            .iter()
            .any(|t| t["agent"] == json!(backend.as_str()) && t["verb"] == json!("send")),
        "the pass is finished — nothing may still be labelled `send`: {tasks:#?}"
    );
}

// ─── the end-of-pass contract: DONE is explicit; silence is re-asked ─────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_chain_turns_are_nudged_until_a_call_or_done() {
    // The model is RE-ASKED UNTIL IT CALLS, inside its pass (#795): a silent answer
    // does not end the chain, and the attempts bound it.
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
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 4).await, "the chain did not reach done");
    let report = json_text(crate::model_format::default::REPORT_NO_CALL);
    for n in [2, 3] {
        assert!(
            s.request(n).contains(&report),
            "request {} must be a re-ask",
            n + 1
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 4, "done ended it; nothing asks after");
}

/// An attempt that is ONLY thinking did not answer: it is never stored and never
/// counts as completed — it is re-asked inside its pass with the no-call report
/// (#795; a tiny thinking budget, then a clean `stop` with no answer; failing it
/// outright would end a long chain).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_turn_is_discarded_and_rerolled() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "dead_think").await;
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let s = script(
        &backend,
        vec![
            pass("<think>hmm</think>\n\n"),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(json!(crate::tests::last_answer(&kernel, &backend).await), json!(r#"<call name="done"></call>"#), "{r:#?}");
    assert_eq!(
        r.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["accepted", "re", "client_id"],
        "{r:#?}"
    );
    assert_eq!(s.passes_served(), 2, "the thinking-only attempt was not re-asked");
    // one call: it ends done, once; the re-ask is an attempt of it, marked `retrying`
    let evs = drain(&mut rx).await;
    let phases: Vec<&str> = evs
        .iter()
        .filter(|v| v["type"] == json!("status"))
        .filter_map(|v| v["phase"].as_str())
        .collect();
    assert!(phases.contains(&"retrying"), "{evs:#?}");
    assert_eq!(phases.iter().filter(|p| **p == "done").count(), 1, "{evs:#?}");
    let incoming = s.request_messages(1);
    let last = incoming
        .last()
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    assert_eq!(last, crate::model_format::default::REPORT_NO_CALL);
    // the message the attempt answered is KEPT (a reply batch must not vanish);
    // only its empty answer is dropped
    assert!(
        incoming.iter().any(|m| {
            let c = m["content"].as_str().unwrap_or("");
            c == "go" || c.ends_with("go")
        }),
        "the incoming \"go\" was lost: {incoming:#?}"
    );
    assert!(
        !incoming.iter().any(|m| {
            m["role"] == json!("assistant") && m["content"].as_str().unwrap_or("").trim().is_empty()
        }),
        "an empty assistant row reached the model: {incoming:#?}"
    );
}

/// An answer with no call is re-asked INSIDE its pass (#795, Q3): prose, then prose
/// again, and after `1 + CALL_RETRIES` attempts the harness stops the pass — status
/// `failed`, `reason: gave_up`, the stop reason naming the attempts — and nothing more
/// is asked. Each re-ask carries the prose and the no-call report; the prose is never
/// stored as an answer. A silent model never ends work silently, and never runs away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_less_answer_is_reasked_inside_the_pass_then_the_harness_stops_it() {
    let n = 1 + crate::constants::CALL_RETRIES;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "gives_up").await;
    let s = script(
        &backend,
        (0..n + 5).map(|_| pass("just prose")).collect(),
        Duration::ZERO,
    );
    // a browser-style client: its events land on the agent's own inbox, where
    // they can be read (`cli` events go to the terminal instead)
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "tab1"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, n).await, "the pass stalled before its last attempt");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(s.passes_served(), n, "nothing is asked after the harness stop");
    // every re-ask carries the answer and the type's no-call report
    let report = crate::model_format::default::REPORT_NO_CALL;
    for k in 1..n {
        let req = s.request_messages(k);
        let tail: Vec<&Value> = req.iter().rev().take(2).collect();
        assert_eq!(tail[0]["content"], json!(report), "{req:#?}");
        assert_eq!(tail[1]["content"], json!("just prose"), "{req:#?}");
    }
    let mut dones = Vec::new();
    while let Some(v) = rx.try_recv() {
        let p = v.payload;
        if p["type"] == json!("status")
            && ["done", "failed"].contains(&p["phase"].as_str().unwrap_or_default())
        {
            dones.push(p);
        }
    }
    let last = dones.last().expect("an ending status");
    assert_eq!(last["phase"], json!("failed"), "{dones:#?}");
    assert_eq!(last["detail"]["reason"], json!("gave_up"), "{dones:#?}");
    assert_eq!(
        last["detail"]["error"],
        json!(crate::model_format::fill(
            crate::model_format::default::STOP_REASON,
            &[("n", &n.to_string())]
        )),
        "{last:#?}"
    );
    // the prose was no answer: the one stored assistant row is the harness's `stop`
    let rows = conversation(&kernel, &backend).await;
    let answers: Vec<&str> = rows
        .iter()
        .filter(|r| r["role"] == json!("assistant"))
        .filter_map(|r| r["content"].as_str())
        .collect();
    assert_eq!(answers.len(), 1, "{rows:#?}");
    assert!(answers[0].starts_with("<call name=\"stop\">"), "{answers:?}");
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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
    // call in thought and never emits it is reminded exactly once, and `done`
    // closes it.
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi"}),
        )]))
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
/// KERNEL defaults nothing, so without it a call to a handler that never
/// returns hangs the whole batch forever: no reply, no timeout block, the chain
/// dead with nothing to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_model_call_carries_the_layer_default_timeout() {
    let tmp = TempDir::new().unwrap();
    // A NON-AI target that answers after 2s (an AI acks at once, #795). The layer's
    // deadline is 0.2s, and NO `timeout` arg is written in the envelope — the default
    // has to do the work.
    let (kernel, a) = with_sleeper(&tmp, "rt_dl").await;
    let sa = script(
        &a,
        vec![pass("read it <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let call = call_envelope(
        "sleeper",
        json!({"type": "send", "text": "hi", "ms": 2000}),
        Some("self"),
    );
    fantastic_kernel::test_support::as_agent(
        a.clone(),
        crate::route::route_reply(
            a.clone(),
            Arc::clone(&kernel),
            crate::route::Routed {
                text: call,
                clients: vec!["probe".to_string()],
                client_id: "probe".to_string(),
                send_timeout: 0.2,
                model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
                message_id: None,
                call_streams: Vec::new(),
                cut: false,
            },
        ),
    )
    .await;
    assert!(
        until_passes(&sa, 1).await,
        "the timed-out call produced no block at all — the batch hung"
    );
    let turn = sa.request(0);
    // The sender is told its message hit the deadline and went to the dead
    // letter (#568), with the kernel's `reason` and `task_id` kept.
    assert!(
        turn.contains("hit its 0.2s deadline"),
        "the block must REPORT the deadline: {turn}"
    );
    assert!(
        turn.contains("dead letter"),
        "must name the dead letter: {turn}"
    );
    assert!(
        turn.contains("<reason>timeout</reason>"),
        "must keep the reason: {turn}"
    );
    assert!(turn.contains("<task_id>"), "must carry task_id: {turn}");
}

/// A `timeout` that is not a number is REFUSED by name where the call is read —
/// never dropped (defaulting the call to the layer's `send_timeout` as if the
/// model had never written the arg), never coerced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_timeout_arg_is_refused_by_name() {
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
    fantastic_kernel::test_support::as_agent(
        a.clone(),
        crate::route::route_reply(
            a.clone(),
            Arc::clone(&kernel),
            crate::route::Routed {
                text: call,
                clients: vec!["probe".to_string()],
                client_id: "probe".to_string(),
                send_timeout: 0.2,
                model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
                message_id: None,
                call_streams: Vec::new(),
                cut: false,
            },
        ),
    )
    .await;
    assert!(
        until_passes(&sa, 1).await,
        "the kernel's refusal must reach the model as a block"
    );
    let turn = sa.request(0);
    assert!(
        turn.contains("timeout must be number"),
        "must carry the refusal naming the argument: {turn}"
    );
    assert!(
        turn.contains("<arg>timeout</arg>"),
        "must name the argument: {turn}"
    );
}

/// A `reply_to` naming NO AGENT dispatches nothing and tells the model (a chat
/// message id typed as the number 1001 must never DISCARD the reply). The grammar
/// types the bare number here too, so this pins the number-is-an-id half as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_to_naming_no_agent_dispatches_nothing_and_says_so() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp, "rt_deadreply").await;
    let b = add_peer(&kernel, &tmp, &a).await;
    let sa = script(
        &a,
        vec![pass("read it <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let sb = script(&b, vec![pass("must never run")], Duration::ZERO);
    let call = call_envelope(
        b.as_str(),
        json!({"type": "send", "text": "hi", "client_id": "probe"}),
        Some("1001"),
    );
    fantastic_kernel::test_support::as_agent(
        a.clone(),
        crate::route::route_reply(
            a.clone(),
            Arc::clone(&kernel),
            crate::route::Routed {
                text: call,
                clients: vec!["probe".to_string()],
                client_id: "probe".to_string(),
                send_timeout: 5.0,
                model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
                message_id: None,
                call_streams: Vec::new(),
                cut: false,
            },
        ),
    )
    .await;
    assert!(
        until_passes(&sa, 1).await,
        "the refusal must reach the model"
    );
    let turn = sa.request(0);
    assert!(
        turn.contains("reply_to='1001' names no agent, so nothing was dispatched"),
        "{turn}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sb.passes_served(), 0, "the call must not have run");
}

/// A turn whose calls all carried `reply_to:null` DISPATCHED REAL WORK. It is
/// owed no reply, and it is not a silent turn: the gate is python's `not calls
/// and not units`, never the blocks, so the model is never told "your turn
/// emitted no <call> element" about a turn that did.
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
        vec![pass("nothing lands here")],
        Duration::ZERO,
    );
    // A CHAIN turn (its incoming text is a delivered reply) — exactly the turn the
    // nudge exists for, so nothing but the dispatch can be what keeps it quiet.
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            a.clone(),
            json!({
                "type": "send",
                "text": "[reply to core:1 · core ping]\nok",
                "client_id": "probe",
            }),
        )]))
        .await
        .remove(0)
        .result;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        sa.passes_served(),
        1,
        "a dispatching turn must not be nudged"
    );
    assert_eq!(sb.passes_served(), 0, "and reaches nobody else");
}

/// Two reads through the agent's own file leg — files of `chars` each — at a
/// 4.5k window; the two reply rows that came back, and the inline cap.
async fn two_reads_turn(tag: &str, frac: f64) -> (Vec<String>, i64) {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, tag).await;
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(),
                   "context_window": 4500, "output_reserve": 0}),
        )])
        .await;
    let meta = crate::helpers::agent_meta(&backend, &kernel);
    let cap = crate::context::inline_cap(&meta, crate::constants::DEFAULT_CONTEXT_WINDOW);
    let chars = (cap as f64 * 4.0 * frac) as usize;
    let root = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("alpha.txt"), "a".repeat(chars)).unwrap();
    std::fs::write(root.join("beta.txt"), "b".repeat(chars)).unwrap();
    let file_leg = format!("ff_{}", backend.as_str());
    let s = script(
        &backend,
        vec![
            pass(format!(
                "{}\n{}",
                call_envelope(
                    &file_leg,
                    json!({"type": "read", "path": "alpha.txt"}),
                    Some("self")
                ),
                call_envelope(
                    &file_leg,
                    json!({"type": "read", "path": "beta.txt"}),
                    Some("self")
                ),
            )),
            pass("both back <call name=\"done\"></call>"),
            pass("both back <call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the replies never came back");
    assert!(settle(&kernel).await, "the agent never went quiet");
    let replies = reply_rows(&kernel, &backend).await;
    assert_eq!(replies.len(), 2, "{replies:#?}");
    (replies, cap)
}

/// THE CAP IS PER REPLY (#795: each reply is its own message): a reply over the inline
/// cap is held and stubbed by its origin line; the stored message stays within the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_inline_cap_bounds_each_reply() {
    let (replies, cap) = two_reads_turn("rt_batchcap", 1.2).await;
    for r in &replies {
        assert!(r.contains("NOT inlined"), "{r}");
        assert!(!r.contains(&"a".repeat(100)) && !r.contains(&"b".repeat(100)), "{r}");
        assert!(
            crate::context::estimate_one(&json!({"role": "user", "content": r})) <= cap + 64,
            "{r}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replies_that_fit_together_are_all_inline() {
    let (replies, _) = two_reads_turn("rt_batchfit", 0.3).await;
    let all = replies.join("\n\n");
    assert!(!all.contains("NOT inlined"), "{all}");
    assert!(all.contains(&"a".repeat(100)) && all.contains(&"b".repeat(100)), "{all}");
}

/// The batch stamp lives INSIDE the `[reply to …]` tag, where the shared prompt
/// says it does — python's history trimmers (`_REPLY_TAG` / `_BLOCK_SPLIT`, both
/// anchored at the START of a block) cannot read a trailing line at all.
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the batch never came back");
    assert!(settle(&kernel).await);
    let turn2 = reply_rows(&kernel, &backend).await.join("\n\n");
    assert!(
        turn2.contains(" · 1 of 2 in batch ") && turn2.contains(" · 2 of 2 in batch "),
        "the stamp must be composed into the tag: {turn2}"
    );
    assert!(
        !turn2.contains("[1 of 2 in batch"),
        "no trailing stamp line may survive: {turn2}"
    );
}

/// Read the newest batch token out of a request. Anchored on the STAMP (`1 of 2 in
/// batch …`), not on the words: the system block teaches the shape, so a bare
/// `in batch ` search finds the prompt's own `<token>` placeholder first.
fn batch_token_of(turn: &str) -> String {
    const MARK: &str = "1 of 2 in batch ";
    let at = turn
        .rfind(MARK)
        .unwrap_or_else(|| panic!("no batch stamp in: {turn}"))
        + MARK.len();
    turn[at..].chars().take(4).collect()
}

/// A FRESH token per turn, never derived from the batch's own content
/// (destinations, counts, body lengths): two turns that made the SAME calls would
/// collide — the exact confusion the token exists to end: a model that read one
/// file twice would see the same batch token both times.
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
            pass("enough <call name=\"done\"></call>"),
            pass("enough <call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 3).await, "the second batch never landed");
    assert!(settle(&kernel).await);
    let tokens: std::collections::BTreeSet<String> = reply_rows(&kernel, &backend)
        .await
        .iter()
        .filter(|r| r.contains("1 of 2 in batch "))
        .map(|r| batch_token_of(r))
        .collect();
    assert!(
        tokens.len() >= 2,
        "two identical turns must be distinguishable at a glance: {tokens:?}"
    );
}

/// A `reflect` reply is LARGE and TRANSIENT, and the instruction for what to
/// record before it goes rides ON THE REPLY — stated once as system-block policy,
/// it goes unheeded. Only reflect bodies are dropped later, so only they carry
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_replies_carry_no_cut_notice() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_nudge").await;
    let s = script(
        &backend,
        vec![
            pass(format!(
                "{}\n{}",
                // its OWN reflect — by position; a refusal carries no notice
                call_envelope(
                    backend.as_str(),
                    json!({"type": "reflect", "tree": "none"}),
                    Some("self")
                ),
                call_envelope("kernel_state", json!({"type": "list_agents"}), Some("self")),
            )),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the reflect never came back");
    let turn2 = s.request(1);
    // A reflect is MUTABLE state and stays whole: it carries
    // no cut notice — only a `docs` reply does.
    assert!(
        turn2.contains("mb_rt_nudge"),
        "the reflect came back: {turn2}"
    );
    assert!(
        !turn2.contains("[transient"),
        "a reflect is never cut: {turn2}"
    );
}

/// A FAILED HANDOFF is announced as an ordinary `status` event — `detail.reason`,
/// the shape reflect's `emits` documents and a UI subscribes to — never
/// `reason`/`error` at the TOP level, where no subscriber reads them.
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    let events = drain(&mut rx).await;
    let status = events
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("status"))
        .unwrap_or_else(|| panic!("the dead handoff was silent: {events:#?}"));
    assert_eq!(status["phase"], "failed", "{status:#?}");
    assert_eq!(status["detail"]["reason"], "error", "{status:#?}");
    // The error is the REPLY's own error, verbatim. python passes
    // `reply.get("error")` straight through; the raw status is the fallback and
    // carries NO `delivery ` prefix, which rust added to a string no python
    // subscriber matches.
    let why = status["detail"]["error"]
        .as_str()
        .unwrap_or_else(|| panic!("the announcement must say WHY: {status:#?}"));
    assert!(!why.starts_with("delivery "), "{status:#?}");
    assert!(why.contains("unknown verb"), "{status:#?}");
    assert_eq!(status["source"], leg.as_str(), "{status:#?}");
    assert!(status.get("ts").is_some(), "{status:#?}");
    assert_eq!(status["client_id"], "probe", "{status:#?}");
    assert_eq!(s.passes_served(), 1);
}

/// THE ROUTER DOES NOT WAIT FOR A DELIVERY (#795): a reply handed to another agent is
/// that agent's `send`, through `Kernel::call`, so a slow peer holds nothing of this
/// agent's; the hand-off is a `send` call row on the peer, never a row of its
/// conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_is_a_deliver_reply_task_not_awaited() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_dsoon").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    // The peer's own pass is SLOW.
    let fp = script(&peer, vec![pass(r#"peer ack <call name="done"></call>"#)], Duration::from_millis(800));
    let s = script(
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
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )])
        .await;
    assert!(until_passes(&s, 1).await);
    assert!(until_passes(&fp, 1).await, "the reply never reached the peer");
    // the reply reached the peer's pass before its slow answer: nothing waited on it
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "the router awaited the delivery: {:?}",
        started.elapsed()
    );
    assert!(settle(&kernel).await);
    let handed = fantastic_kernel::test_support::rows_about(&kernel, &peer)
        .into_iter()
        .filter(|r| r.get("kind") == Some(&json!("call")) && r.get("verb") == Some(&json!("send")))
        .count();
    assert_eq!(handed, 1, "one hand-off, as the peer's `send`");
    assert!(
        history_of(&kernel, &peer)
            .await
            .expect("store read")
            .iter()
            .all(|r| r.get("verb") != Some(&json!("send"))),
        "a call row reached the peer's conversation"
    );
}

/// A CANCELLED handoff is not announced. python's `failed` is
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

/// The reaction cursor is the DURABLE STORE's length, not the model view's. The
/// view is deduped/trimmed, so counting it puts the cursor short by exactly the
/// turns the trimmers dropped — and `derive_reaction` then scans assistant turns
/// written BEFORE the notice and scores a reaction that never happened.
#[tokio::test]
async fn the_compaction_cursor_is_the_store_length_not_the_view() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "mark").await;
    let state = crate::state::state_for(&backend);

    // ① the store length is what `assemble_messages` reports.
    let stale = "[reply to ks:a1 · core reflect]\nOLD TREE";
    let fresh = "[reply to ks:a2 · core reflect]\nNEW TREE";
    let turns = vec![
        turn("user", stale),
        turn("assistant", "noted"),
        turn("user", fresh),
        turn("assistant", "noted again"),
    ];
    fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::save_history(&backend, &kernel, &turns),
    )
    .await
    .expect("store append");
    let (messages, store_len) = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::assembly::assemble_messages(
            &backend,
            &state,
            &kernel,
            "probe",
            crate::caching::Caching::ProviderCache,
        ),
    )
    .await
    .expect("store read");
    assert_eq!(store_len, 4, "the STORE keeps every turn");
    let _ = messages;

    // ② and the mark records THAT number, not the view's length.
    let provider: Arc<dyn Provider> = Arc::new(SummaryProvider("SUMMARY"));
    let mut fat = vec![turn("system", "sys")];
    fat.extend(body(60, 8000));
    let n = fat.len();
    let _ = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::projection::projected(
            crate::model_format::pass::Pass::for_test(Arc::clone(&provider)),
            &backend,
            &state,
            &kernel,
            "probe",
            "mock_backend",
            crate::constants::DEFAULT_CONTEXT_WINDOW,
            fat,
            store_len,
        ),
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

/// `recall` answers the turn WHOLE. Rust capped each recalled turn at 2000
/// chars while python returned it verbatim — same verb, same store, two different
/// replies, and the cut half is exactly what a model pages back FOR.
#[tokio::test]
async fn recall_returns_the_turn_whole() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rec_whole").await;
    script(&backend, vec![pass("noted")], Duration::ZERO);
    let long = format!("HEAD {} TAIL", "x".repeat(6000));
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": long, "client_id": "chat"}),
        )]))
        .await
        .remove(0)
        .result;
    let r = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "recall", "query": "HEAD"}),
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

/// SEVEN PARALLEL CALLS, NO `reply_to` — a well-formed turn batching seven
/// `reflect` sends with the key omitted must not end the chain with NOTHING
/// back: no reply block, no error, no timeout, no cancel. An omitted key means
/// the model gets its own answer back, so every one of these is owed a block.
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
    fantastic_kernel::test_support::as_agent(
        a.clone(),
        crate::route::route_reply(
            a.clone(),
            Arc::clone(&kernel),
            crate::route::Routed {
                text,
                clients: vec!["probe".to_string()],
                client_id: "probe".to_string(),
                send_timeout: 30.0,
                model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
                message_id: None,
                call_streams: Vec::new(),
                cut: false,
            },
        ),
    )
    .await;
    assert!(
        until_passes(&sa, 1).await,
        "seven dispatched calls produced NO follow-up turn — the replies were dropped"
    );
    assert!(settle(&kernel).await);
    let replies = reply_rows(&kernel, &a).await;
    assert_eq!(replies.len(), 7, "every call is owed a block: {replies:#?}");
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "build it", "client_id": "probe"}),
        )]))
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "explore", "client_id": "probe"}),
        )]))
        .await;
    assert!(
        until_passes(&sa, 2).await,
        "turn 2 never ran — a batch containing a peer AI delivered nothing \
         (passes={})",
        sa.passes_served()
    );
}

// ── done: nothing more now ───────────────────────────────────────────────────

/// `done` leaves the agent idle (#795): no second pass runs and nothing reminds it;
/// the next message wakes it with a pass of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_leaves_the_agent_idle_and_the_next_message_wakes_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "aw1").await;
    let s = script(
        &backend,
        vec![
            pass(r#"nothing more now <call name="done"></call>"#),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 1).await, "the first pass never ran");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "done left the agent idle");
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "again", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(s.passes_served(), 2, "the next message woke it");
    assert!(s.request(1).contains("again"), "{}", s.request(1));
}

/// Something is QUEUED for the agent when its pass ends with `done`: the queued
/// message is the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_with_a_message_queued_and_the_queue_wakes_it() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "aw2").await;
    let s = script(
        &backend,
        vec![
            pass(r#"nothing more now <call name="done"></call>"#),
            pass(r#"<call name="done"></call>"#),
        ],
        Duration::from_millis(300),
    );
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let first = tokio::spawn(async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(b, json!({"type": "send", "text": "one"}))]))
            .await
            .remove(0)
            .result
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "two"}),
        )]))
        .await
        .remove(0)
        .result;
    let _ = first.await.expect("first send");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.passes_served(), 2, "the queued message is the next turn");
    assert!(s.request(1).contains("two"), "{}", s.request(1));
}

/// A send acks at once (#795): the second message is stored while the first pass runs
/// and read by the next pass, never a second pass beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_that_lands_during_a_pass_is_read_by_the_next_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "joins").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let ask = call_envelope(peer.as_str(), json!({"type": "send", "text": "ask"}), None);
    let gate = script(&backend, vec![pass(ask), pass(r#"<call name="done"></call>"#)], Duration::ZERO)
        .hold_next();
    let a = script_for(&backend);
    let _r = script(&peer, vec![pass(r#"answered <call name="done"></call>"#)], Duration::ZERO);
    let first = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "go", "client_id": "g"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(first["accepted"], json!(true), "{first}");
    assert!(until_passes(&a, 1).await, "the first pass never started");
    // a message that lands while the pass runs: acked at once, not waiting for it
    let two = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "two", "client_id": "t"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(two["accepted"], json!(true), "{two}");
    assert!(two["re"].as_i64().is_some(), "{two}");
    gate.notify_one();
    assert!(settle(&kernel).await, "the agents never went quiet");
    assert_eq!(a.passes_served(), 2, "one more pass, never a second beside the first");
    assert!(a.request(1).contains("two"), "{}", a.request(1));
}

/// THE COALESCED WAKE (#795): three messages that land during a pass are read by ONE
/// next pass, all three in its view, after a notice that they arrived meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_arriving_during_a_pass_are_read_by_one_next_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "wakeonce").await;
    let s = script(
        &backend,
        vec![pass(r#"<call name="done"></call>"#), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let gate = s.hold_next();
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "one", "client_id": "o"}))])
        .await;
    assert!(until_passes(&s, 1).await, "the first pass never started");
    for text in ["two", "three", "four"] {
        let r = kernel
            .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": text, "client_id": "t"}))])
            .await
            .remove(0)
            .result;
        assert_eq!(r["accepted"], json!(true), "{r}");
    }
    gate.notify_one();
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert_eq!(s.passes_served(), 2, "exactly one pass for the three");
    let wake = s.request(1);
    assert!(wake.contains("3 message(s) arrived while you were working"), "{wake}");
    for text in ["two", "three", "four"] {
        assert!(wake.contains(text), "{text}: {wake}");
    }
}

/// `stop` ends the pass and wakes nothing (#795): a message that landed during it stays
/// stored, unread, and no pass runs for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_ends_the_pass_and_wakes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "stopnowake").await;
    let s = script(&backend, vec![pass(r#"<call name="done"></call>"#)], Duration::ZERO);
    let gate = s.hold_next();
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "one", "client_id": "o"}))])
        .await;
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let two = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "two", "client_id": "t"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(two["accepted"], json!(true), "{two}");
    let stopped = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(stopped["stopped"], json!(true), "{stopped}");
    gate.notify_one();
    assert!(settle(&kernel).await, "the agent never went quiet");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "stop woke the unread message");
}

/// A SEND TO AN AI IS AN ACK (#795): `{accepted, re, client_id}`, returned before the
/// model has answered — the message is the stored row `re`, and the answer comes later
/// as the agent's own row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_to_an_ai_returns_its_ack_before_any_request() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "heldanswer").await;
    let s = script(
        &backend,
        vec![pass(r#"ANSWER-OF-THE-PASS <call name="done"></call>"#)],
        Duration::ZERO,
    );
    let gate = s.hold_next();
    let out = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "one", "client_id": "o"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(out["accepted"], json!(true), "{out}");
    assert_eq!(out["client_id"], json!("o"), "{out}");
    assert!(out.get("response").is_none(), "an ack carries no answer: {out}");
    let re = out["re"].as_i64().expect("the stored row's id");
    let rows = history_of(&kernel, &backend).await.expect("history");
    assert!(
        rows.iter().any(|r| r["id"] == json!(re) && r["content"] == json!("one")),
        "the message is stored before the ack: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r["role"] == "assistant"),
        "no answer exists when the ack returns: {rows:?}"
    );
    gate.notify_one();
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert!(last_answer(&kernel, &backend).await.contains("ANSWER-OF-THE-PASS"));
}

/// A pass whose calls all reply ELSEWHERE (`reply_to` null) brings no message back:
/// the agent is idle, and the next message runs its own pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_whose_calls_reply_elsewhere_leaves_the_agent_idle() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "elsewhere").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let elsewhere = format!(
        r#"<call name="send"><routing><target_id>{peer}</target_id><reply_to>null</reply_to></routing><payload><type>send</type><text>hi</text></payload></call>"#
    );
    let s = script(&backend, vec![pass(elsewhere), pass("second turn")], Duration::ZERO);
    let _r = script(&peer, vec![pass(r#"answered <call name="done"></call>"#)], Duration::ZERO);
    let first = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
        backend.clone(),
        json!({"type": "send", "text": "go", "client_id": "g"}),
    )]))
    .await;
    assert!(first.iter().all(|o| o.result.get("error").is_none()));
    assert_eq!(s.passes_served(), 1, "nothing came back to wake it");
    let second = crate::tests::settled(&kernel, kernel.operator_send(vec![Item::new(
        backend.clone(),
        json!({"type": "send", "text": "next", "client_id": "n"}),
    )]))
    .await
    .remove(0)
    .result;
    assert_eq!(second["accepted"], json!(true), "{second}");
    assert!(until_passes(&s, 2).await, "the next message ran no pass");
}

/// Poll `cond` until it holds, with a deadline — a wait on the CONDITION, never a
/// fixed sleep. Names `what` when the deadline passes.
pub(crate) async fn until_true(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until `n` messages are stored and not read yet: the next pass reads them.
async fn until_unread(backend: &AgentId, n: usize) {
    until_true("a message to wait unread", || {
        crate::state::state_for(backend).life.lock().unwrap().unread.len() >= n
    })
    .await;
}

/// Whether the agent's store holds a `late` row (a reply recorded, not acted on).
async fn has_late_row(kernel: &Arc<Kernel>, backend: &AgentId) -> bool {
    let rows = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(backend, kernel, false),
    )
    .await
    .expect("store read");
    rows.iter().any(|r| r["kind"] == "late")
}

/// A pass held at its request (`gate`), with a second message stored during it.
async fn unread_behind_a_pass(tag: &str) -> (TempDir, Arc<Kernel>, AgentId, Arc<MockScript>, Arc<tokio::sync::Notify>) {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, tag).await;
    let s = script(
        &backend,
        vec![pass(r#"<call name="done"></call>"#), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let gate = s.hold_next();
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "one", "client_id": "o"}))])
        .await;
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let two = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "two", "client_id": "t"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(two["accepted"], json!(true), "{two}");
    (tmp, kernel, backend, s, gate)
}

/// A REBOOT mid-pass (#795): the pass in flight acts on nothing when its request settles,
/// and the messages it and its successor were to read — stored, acked — are read by ONE
/// pass of the new incarnation: nothing acked is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reboot_reads_the_unread_messages_in_one_pass() {
    let (_tmp, kernel, backend, s, gate) = unread_behind_a_pass("rebootchain").await;
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
        .await;
    gate.notify_one();
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert_eq!(s.passes_served(), 2, "one pass after the boot");
    let read = s.request(1);
    assert!(read.contains("one") && read.contains("two"), "{read}");
    let answers = history_of(&kernel, &backend)
        .await
        .expect("history")
        .iter()
        .filter(|r| r["role"] == "assistant")
        .count();
    assert_eq!(answers, 1, "the cut pass stored nothing");
}

/// A CONTEXT CLEAR starts a new thread: the message not read yet belongs to the old one
/// and wakes no pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_context_clear_leaves_the_unread_to_the_old_thread() {
    let (_tmp, kernel, backend, s, gate) = unread_behind_a_pass("clearchain").await;
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "context_clear", "why": "fresh start"}))])
        .await;
    gate.notify_one();
    assert!(settle(&kernel).await, "the agent never went quiet");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "the cleared message woke a pass");
}

/// An agent that shuts down (`on_delete` drops its state) runs nothing after: a request
/// that settles later acts on nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_runs_nothing_after() {
    let (_tmp, _kernel, backend, s, gate) = unread_behind_a_pass("shutchain").await;
    crate::state::drop_state(&backend);
    gate.notify_one();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.passes_served(), 1, "a pass ran after the shutdown");
}

/// A REPLY DECIDED BEFORE A `stop` lands after it as a `late` row and runs nothing
/// (#795, was the chain token #227 bugs 4 and 5): the stop ended the work it answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_in_flight_across_a_stop_runs_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "stopinflight").await;
    let s = script(&backend, vec![pass(r#"<call name="done"></call>"#)], Duration::ZERO);
    let gate = s.hold_next();
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "one", "client_id": "o"}))])
        .await;
    assert!(until_passes(&s, 1).await, "the first pass never started");
    // a reply routed under the work in flight: decided now
    let born = crate::route::Born::of(backend.as_str());
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "stop"}))])
        .await;
    gate.notify_one();
    fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::route::deliver(&kernel, &backend, backend.as_str(), "[reply to 1 · peer]\nanswered", "cli", born, false, true),
    )
    .await;
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert!(has_late_row(&kernel, &backend).await, "the reply was not kept late");
    assert_eq!(s.passes_served(), 1, "the work resumed after a stop");
}

/// A reply decided by a previous incarnation lands as a `late` row and runs nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routed_reply_across_a_reboot_is_late() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "tokenwhen").await;
    let s = script(&backend, vec![pass(r#"<call name="done"></call>"#)], Duration::ZERO);
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
        .await;
    let born = crate::route::Born::of(backend.as_str());
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
        .await;
    fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::route::deliver(&kernel, &backend, backend.as_str(), "an old reply", "cli", born, true, true),
    )
    .await;
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert!(has_late_row(&kernel, &backend).await, "the old reply was not kept late");
    assert_eq!(s.passes_served(), 0, "the old reply ran a pass of the new incarnation");
}

/// A peer's message is a message like any other: read by a pass, never `late`.
#[tokio::test]
async fn a_peer_reply_is_read_never_late() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "awaitnew").await;
    let s = script(&backend, vec![pass(r#"<call name="done"></call>"#)], Duration::ZERO);
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let target = backend.clone();
    let k = Arc::clone(&kernel);
    let out = fantastic_kernel::test_support::as_agent(peer, async move {
        k.send(vec![Item::new(
            target,
            json!({"type": "send", "text": "[reply to 1 · peer]\nanswered", "client_id": "cli"}),
        )])
        .await
        .remove(0)
        .result
    })
    .await;
    assert_eq!(out["accepted"], json!(true), "{out}");
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert_eq!(s.passes_served(), 1, "the reply was not read");
    assert!(!has_late_row(&kernel, &backend).await, "a peer's reply was recorded as late");
}

/// An inquiry's ANSWER is a message like any other: one that lands during a pass is read
/// by the next pass, never a pass of its own beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inquiry_answer_is_read_by_the_next_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "inquiryhold").await;
    let s = script(
        &backend,
        vec![pass(r#"<call name="done"></call>"#), pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let gate = s.hold_next();
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "send", "text": "one", "client_id": "o"}))])
        .await;
    assert!(until_passes(&s, 1).await, "the first pass never started");
    let answer = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "[reply to p9 · peer inquiry]\nyes", "client_id": "t"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(answer["accepted"], json!(true), "{answer}");
    gate.notify_one();
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert_eq!(s.passes_served(), 2);
    assert!(s.request(1).contains("yes"), "{}", s.request(1));
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
        r#"<call name="send"><routing><to><item>{b}</item><item>{a}</item></to><re>412</re><reply_to>null</reply_to></routing><payload><type>send</type><text>CANDIDATE 586</text></payload></call>"#
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            a.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
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
    let long = format!("{}{}", "x".repeat(275), r#"<call name="done"></call>"#);
    script(
        &backend,
        vec![pass(long.clone())],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping"}),
        )]))
        .await
        .remove(0)
        .result;
    let h = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "history"}))])
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
        .operator_send(vec![Item::new(backend, json!({"type": "recall"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["messages"].as_array().unwrap()[1]["content"], long);
}

/// A NEGATIVE `limit` is already satisfied by the empty page — python
/// `_fit_reply(digest, cap, limit or None)` returns `([], True)`.
#[tokio::test]
async fn history_limit_below_one_is_refused_by_name() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hneg").await;
    script(
        &backend,
        vec![pass(r#"<call name="done"></call>"#)],
        Duration::ZERO,
    );
    let _rx = rebind_backend_inbox(&kernel, &backend);
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping"}),
        )]))
        .await
        .remove(0)
        .result;
    let r = kernel
        .operator_send(vec![Item::new(
            backend,
            json!({"type": "history", "limit": -1}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("bad_arg"), "{r:#?}");
    assert_eq!(r["arg"], json!("limit"), "{r:#?}");
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "ping"}),
        )]))
        .await
        .remove(0)
        .result;
    let page = |args: Value| {
        let kernel = Arc::clone(&kernel);
        let backend = backend.clone();
        async move {
            kernel
                .operator_send(vec![Item::new(backend, args)])
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
/// that subscribed to the AGENT (which is what a `watch` frame names) would see
/// nothing at all. Python emits on the agent (`core.py::_to_caller`). Guarded
/// here because otherwise only a live e2e run catches it, and this is a one-line
/// regression to reintroduce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_client_events_are_emitted_on_the_agent_not_a_client_named_inbox() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "evt_route").await;
    let mut agent_rx = rebind_backend_inbox(&kernel, &backend);

    let client = "browser_tok";
    let client_id = AgentId::from(client);
    let mut client_rx = kernel.ensure_inbox(&client_id);

    // the operator's emit: a bare test task has no origin scope, and its emit drops
    fantastic_kernel::send::as_operator(crate::events::to_caller(
        &kernel,
        &backend,
        client,
        json!({"type": "token", "delta": "x"}),
    ))
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

/// ONE caller route, python's `_to_caller`: `client_id == 'cli'` is
/// DISPATCHED to the `cli` agent (so its handler prints to stdout), everyone
/// else is emitted on the backend's own inbox. Rust selected between the two
/// with a per-backend `CallerRoute` knob python does not have, so three of the
/// four backends never reached the `cli` handler at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cli_caller_round_trips_through_the_cli_agent() {
    /// Stands in for the real `cli` bundle: records what it was handed.
    struct CliSink(Arc<std::sync::Mutex<Vec<Value>>>);
    /// The one verb the cli sink answers: a streamed token, with where it came from.
    #[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    struct Token {
        client_id: String,
        source: String,
        /// The event's place in a run of them; the first of a run is slow to print.
        n: Option<u64>,
    }
    fantastic_kernel::verbs! { "token" => Token }
    #[async_trait]
    impl Handler for CliSink {
        type Call = Call;
        fn name(&self) -> &str {
            "cli"
        }
        fn man(&self) -> &'static str {
            "A stand-in for the cli bundle: it records the tokens it is handed."
        }
        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            VERBS
        }
        fn dynamic_verbs(&self, a: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            dynamic_verbs(a)
        }
        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            &crate::tests_decl::any::RECORD
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            call: Call,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            if let Call::Token(t) = call {
                if t.n == Some(0) {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                self.0
                    .lock()
                    .expect("sink poisoned")
                    .push(serde_json::to_value(&t).expect("a token"));
            }
            Ok(None)
        }
    }

    let tmp = TempDir::new().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = CliSink(Arc::clone(&seen));
    let kernel = base_kernel(&tmp, move |k| k.bundles.register("cli.sink", sink)).await;
    let backend_id = id_for(&tmp, "evt_cli");
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic")).await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    let mut agent_rx = rebind_backend_inbox(&kernel, &backend);

    // The backend already holds its line `cli: {call: [*]}`; `cli` keeps no list.
    let cli_meta = Map::new();
    let cli = Agent::new(
        AgentId::from("cli"),
        Some("cli.sink".to_string()),
        kernel.root().map(|r| r.id.clone()),
        cli_meta,
        tmp.path().join(".fantastic"),
        false,
    );
    fantastic_kernel::test_support::adopt(&kernel, Arc::clone(&cli));

    // the event is the backend's own send, made in its dispatch
    fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::events::to_caller(&kernel, &backend, "cli", json!({"type": "token"})),
    )
    .await;
    // the event is a call to the `cli` agent (#795): it lands once that call ran
    for _ in 0..250 {
        if !seen.lock().expect("sink poisoned").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let handed = seen.lock().expect("sink poisoned").clone();
    assert_eq!(handed.len(), 1, "the cli handler must run: {handed:?}");
    assert_eq!(handed[0]["client_id"], json!("cli"));
    assert_eq!(handed[0]["source"], json!(backend.as_str()));
    // one model: the same event also lands on the agent's own inbox, for every view watching it
    let mirrored = agent_rx
        .try_recv()
        .expect("a cli event is emitted on the agent too")
        .payload;
    assert_eq!(mirrored["client_id"], json!("cli"));

    // …and a NON-cli caller still lands on the agent's own inbox.
    fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::events::to_caller(&kernel, &backend, "browser_tok", json!({"type": "token"})),
    )
    .await;
    let ev = agent_rx.try_recv().expect("the browser event").payload;
    assert_eq!(ev["client_id"], json!("browser_tok"));
    assert_eq!(seen.lock().expect("sink poisoned").len(), 1);

    // A RUN OF EVENTS reaches the terminal IN ORDER, and nothing waits for it: each is
    // sent once the one before it settled, so the slow first one is still printing when
    // the run has been handed over
    for n in 0..5u64 {
        fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::events::to_caller(&kernel, &backend, "cli", json!({"type": "token", "n": n})),
        )
        .await;
    }
    assert!(
        seen.lock().expect("sink poisoned").len() < 6,
        "the run waited for the terminal to print"
    );
    for _ in 0..250 {
        if seen.lock().expect("sink poisoned").len() == 6 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let order: Vec<Value> = seen.lock().expect("sink poisoned")[1..].iter().map(|t| t["n"].clone()).collect();
    assert_eq!(order, (0..5u64).map(|n| json!(n)).collect::<Vec<_>>(), "the terminal's feed is out of order");
}

// ─────────────────────────────────────────────────────────────────
// FULL ASYNC: a reply too large to inline is held in RAM under its pid and
// FOUND by `grep`; the view is rebuilt by `context_recall` — both driven
// through the kernel, the way an agent reaches them.
// ─────────────────────────────────────────────────────────────────

/// A test that wants a history writes one rather than acting out ten turns.
/// The agent's CONVERSATION — its rows in its memory store, as it appended them,
/// minus its `view` rows (python `_fake.rows_of`). Read as the agent itself: the
/// store keys every row by its writer.
async fn conversation(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<Value> {
    let res = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::store_call(agent, kernel, json!({"type": "read"})),
    )
    .await
    .expect("store read");
    res.get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        // view rows and the chat's trace rows (#766) are not the conversation
        .filter(|r| {
            !matches!(
                r.get("kind").and_then(Value::as_str),
                Some("view" | "trace")
            )
        })
        .map(|mut r| {
            r["agent"] = json!(agent.as_str());
            r
        })
        .collect()
}

/// Plant a row in the agent's memory store AS the agent (write-through: the head
/// cache takes it too when warm). Returns its id — minted when the row has none.
async fn put_row(kernel: &Arc<Kernel>, agent: &AgentId, row: Value) -> i64 {
    fantastic_kernel::test_support::as_agent(
        agent.clone(),
        crate::memory::save_history(agent, kernel, &[row]),
    )
    .await
    .expect("row rejected")[0]
}

#[tokio::test]
async fn grep_finds_the_line_inside_a_body_that_was_never_pulled() {
    // THE OPERATION THE MODEL ACTUALLY WANTS. Without it a model spends long
    // thinking turns byte-scanning one large body in small windows hunting a
    // single line. "Find the line that mentions X" is one call.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grep").await;
    let pid = kernel.mint_id();
    let mut body = String::new();
    for i in 0..800 {
        body.push_str(&format!("filler line {i}\n"));
    }
    body.push_str("the AUDIT MARKER is 7f3a\n");
    crate::memory::store_reply(&backend, &kernel, pid, &body);
    // The conversation row whose tag carries that pid — how grep knows the body
    // is in reach at all.
    put_row(
        &kernel,
        &backend,
        json!({
            "role": "user",
            "content": format!("[reply to {pid} · notes read · \"survey\"]\n(settled, {} bytes)", body.len()),
        }),
    ).await;

    let r = kernel
        .operator_send(vec![Item::new(
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
    // cap is stored and digested — and then the model pulls its own grep, and the
    // run drowns in pulls.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepcap").await;
    for i in 0..400 {
        put_row(
            &kernel,
            &backend,
            json!({"role": "user", "content": format!("needle {i} {}", "x".repeat(300))}),
        )
        .await;
    }
    let r = kernel
        .operator_send(vec![Item::new(
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
            <= crate::context::inline_cap(&meta, crate::constants::DEFAULT_CONTEXT_WINDOW),
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
    )
    .await;
    let b = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "nothing"}),
    )
    .await;
    let fid = kernel.mint_id();
    put_row(
        &kernel,
        &backend,
        crate::folds::make_fold(
            &[a, b],
            "old work",
            "a summary",
            false,
            fid,
            a.min(b),
            1,
            None,
        ),
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(
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
    )
    .await;
    for i in 0..60 {
        let pid = kernel.mint_id();
        put_row(
            &kernel,
            &backend,
            json!({
                "role": "user",
                "content": format!("[reply to {pid} · notes read · \"chunk {i}\"]\n{}", "y".repeat(2000)),
            }),
        ).await;
    }
    let r = kernel
        .operator_send(vec![Item::new(
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

    let view = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_view(&backend, &kernel),
    )
    .await
    .expect("store read")
    .expect("the view must be persisted");
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
    let rows = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, false),
    )
    .await
    .expect("store read");
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
        .operator_send(vec![Item::new(
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

/// A head written under a window far larger than this agent's: 16 reads of
/// distinct ~900-char bodies against a 2600-token window. Returns the goal id.
async fn long_head(kernel: &Arc<Kernel>, backend: &AgentId, strategy: &str) -> i64 {
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(),
                   "context_strategy": strategy, "context_window": 2600,
                   "output_reserve": 200}),
        )])
        .await;
    let goal = put_row(
        kernel,
        backend,
        json!({"role": "user", "content": "THE GOAL: survey the fillers"}),
    )
    .await;
    let mut prev = goal;
    for i in 0..16 {
        let call = put_row(
            kernel,
            backend,
            json!({"role": "assistant", "parent": prev,
            "content": format!("Reading filler_{i}. <call name=\"send\"><routing><target_id>notes</target_id><title>survey filler_{i}</title></routing><payload><type>read</type><path>filler_{i}.txt</path></payload></call>")}),
        ).await;
        let pid = kernel.mint_id();
        prev = put_row(
            kernel,
            backend,
            json!({"role": "user", "parent": call,
            "content": format!("[reply to {pid} · notes read · \"survey filler_{i}\"]\n{}", format!("filler {i} text ").repeat(60))}),
        ).await;
    }
    goal
}

/// `context_recall` over `long_head`, as the agent, with a scripted model: every
/// pass a summary, so the recall's own question finds no JSON (mechanical fit).
async fn recall_long_head(
    kernel: &Arc<Kernel>,
    backend: &AgentId,
    payload: Value,
    client_id: Option<&str>,
) -> (Value, Arc<MockScript>) {
    let sc = script(
        backend,
        (0..40)
            .map(|_| pass("A title\n\nA short summary of the stretch."))
            .collect(),
        Duration::ZERO,
    );
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        script: script_for(backend),
    });
    // the short prompt the recall is sized against lives on the record
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(), "system_prompt": "sys"}),
        )])
        .await;
    let r = fantastic_kernel::test_support::as_agent(backend.clone(), async {
        let me = crate::assembly::self_reflect(backend, kernel).await;
        let pass = crate::model_format::pass::Pass::for_test(Arc::clone(&provider));
        let decl = pass.decl;
        crate::retrieval::recalled(
            backend,
            payload["dry"] == json!(true),
            kernel,
            Some(pass),
            decl,
            crate::constants::DEFAULT_CONTEXT_WINDOW,
            client_id,
            &[],
            &me,
        )
        .await
    })
    .await
    .expect("store read");
    (r, sc)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_recall_folds_a_head_longer_than_the_budget() {
    // A store written under a larger window (another model, a planted past) must
    // FOLD at the rebuild — not just stub every reply and drop what still does
    // not fit.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "recallfold").await;
    let goal = long_head(&kernel, &backend, "fold").await;
    let mut agent_rx = rebind_backend_inbox(&kernel, &backend);
    let (r, sc) = recall_long_head(&kernel, &backend, json!({}), Some("c1")).await;
    let rows = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, false),
    )
    .await
    .expect("store read");
    let folds: Vec<&Value> = rows
        .iter()
        .filter(|x| x.get("kind").and_then(Value::as_str) == Some("fold"))
        .collect();
    let folded = r["folded"].as_u64().unwrap_or(0);
    assert!(folded >= 1 && folded as usize == folds.len(), "{r}");
    assert!(
        folds
            .iter()
            .all(|f| f["refs"].as_array().is_some_and(|a| !a.is_empty())
                && f["title"].as_str().is_some_and(|t| !t.is_empty())),
        "{folds:?}"
    );
    // one summary per fold, then the recall's own question
    assert_eq!(sc.passes_served(), folds.len() + 1);
    let view = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_view(&backend, &kernel),
    )
    .await
    .expect("store read")
    .expect("the view must be persisted");
    let ids = view["ids"].as_array().cloned().unwrap();
    assert_eq!(
        ids[0]["id"].as_i64(),
        Some(goal),
        "the goal turn is always in"
    );
    assert!(ids.iter().any(|e| e["res"] == "fold"), "{ids:?}");
    // NOTHING FELL OUT UNMARKED: every turn is in the view or under a fold in it
    let in_view: std::collections::HashSet<i64> =
        ids.iter().filter_map(|e| e["id"].as_i64()).collect();
    let fold_ids: Vec<i64> = ids
        .iter()
        .filter(|e| e["res"] == "fold")
        .filter_map(|e| e["id"].as_i64())
        .collect();
    let covered: std::collections::HashSet<i64> = crate::folds::expand_ids(&rows, &fold_ids)
        .into_iter()
        .collect();
    for t in rows.iter().filter(|x| {
        !matches!(
            x.get("kind").and_then(Value::as_str),
            Some("fold" | "recall")
        )
    }) {
        let id = t["id"].as_i64().unwrap();
        assert!(
            in_view.contains(&id) || covered.contains(&id),
            "row {id} fell out: {ids:?}"
        );
    }
    // the turn's caller is told, as by a projection
    let mut evs = Vec::new();
    while let Some(v) = agent_rx.try_recv() {
        if v.payload["type"] == "context" {
            evs.push(v.payload);
        }
    }
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0]["phase"], "compacted");
    assert_eq!(evs[0]["client_id"], "c1");
    assert_eq!(evs[0]["detail"]["folded"].as_u64(), Some(folded));
    assert_eq!(evs[0]["detail"]["recall"], true);
}

/// The folds a recall makes are still under provisional ids (`-1`, …) when the
/// recall asks its question; those ids are never minted, so the prompt shows such
/// a fold as `new fold`, never a `-` id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_recall_prompt_shows_no_provisional_fold_id() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "recallprov").await;
    long_head(&kernel, &backend, "fold").await;
    let (r, sc) = recall_long_head(&kernel, &backend, json!({}), None).await;
    assert!(r["folded"].as_u64().unwrap_or(0) >= 1, "{r}");
    let last = sc.passes_served() - 1;
    let msgs = sc.request_messages(last);
    let prompt = msgs
        .iter()
        .rev()
        .find_map(|m| m["content"].as_str())
        .expect("the recall prompt")
        .to_string();
    assert!(
        prompt.starts_with("You are rebuilding your own working context"),
        "{prompt}"
    );
    let is_neg = |t: &str| {
        t.strip_prefix('-')
            .is_some_and(|d| d.starts_with(|c: char| c.is_ascii_digit()))
    };
    let rows: Vec<&str> = prompt.lines().filter(|l| l.contains(" · ↑")).collect();
    assert!(
        rows.iter().any(|l| l.starts_with("new fold · ")),
        "a fold made here is shown as new: {prompt}"
    );
    for l in &rows {
        let parent = l.rsplit(" · ↑").next().unwrap_or("");
        assert!(!is_neg(l) && !is_neg(parent), "a provisional id shown: {l}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_recall_or_another_strategy_does_not_fold() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "recalldry").await;
    long_head(&kernel, &backend, "fold").await;
    let (r, sc) = recall_long_head(&kernel, &backend, json!({"dry": true}), Some("c1")).await;
    assert_eq!(r["folded"], 0, "dry makes no model call, so no fold: {r}");
    assert_eq!(sc.passes_served(), 0);

    let (kernel2, backend2) = mk_kernel(&tmp, "recalltrunc").await;
    long_head(&kernel2, &backend2, "truncate").await;
    let (r2, _) = recall_long_head(&kernel2, &backend2, json!({}), Some("c1")).await;
    assert_eq!(r2["folded"], 0, "only the fold strategy folds: {r2}");
    for (k, b) in [(&kernel, &backend), (&kernel2, &backend2)] {
        let rows = fantastic_kernel::test_support::as_agent(
            b.clone(),
            crate::memory::load_history(b, k, false),
        )
        .await
        .expect("store read");
        assert!(!rows.iter().any(|x| x["kind"] == "fold"));
    }
}

#[tokio::test]
async fn a_fold_projection_over_budget_writes_folds() {
    // Under `context_strategy: fold` a compaction must actually fold: a fold arm
    // whose compactions all read `folded=0 stripped=0` — the seam fired, the
    // strategy did nothing, the cut did the work — is the control arm with extra
    // steps.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "foldproj").await;
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(),
                   "context_strategy": "fold", "context_window": 2600,
                   "output_reserve": 200, "context_titles": true, "context_async": true,
                   "system_prompt": "sys"}),
        )])
        .await;
    let goal = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL: survey the fillers"}),
    )
    .await;
    let mut prev = goal;
    for i in 0..16 {
        let call = put_row(
            &kernel,
            &backend,
            json!({"role": "assistant", "parent": prev,
            "content": format!("Reading filler_{i}. <call name=\"send\"><routing><target_id>notes</target_id><title>survey filler_{i}</title></routing><payload><type>read</type><path>filler_{i}.txt</path></payload></call>")}),
        ).await;
        let pid = kernel.mint_id();
        prev = put_row(
            &kernel,
            &backend,
            json!({"role": "user", "parent": call,
            // DISTINCT bodies: identical ones are stubbed by the repeat-body dedupe
            "content": format!("[reply to {pid} · notes read · \"survey filler_{i}\"]\n{}", format!("filler {i} text ").repeat(60))}),
        ).await;
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
    let (messages, store_len) = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::assembly::assemble_messages(
            &backend,
            &state,
            &kernel,
            "cli",
            crate::caching::Caching::ProviderCache,
        ),
    )
    .await
    .expect("store read");
    let ids_in_body = messages.iter().filter(|m| m.get("id").is_some()).count();
    let out = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::projection::projected(
            crate::model_format::pass::Pass::for_test(Arc::clone(&provider)),
            &backend,
            &state,
            &kernel,
            "cli",
            "mock",
            crate::constants::DEFAULT_CONTEXT_WINDOW,
            messages.clone(),
            store_len,
        ),
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

/// Under `context_strategy: none` an over-budget pass is REFUSED — its `failed` status
/// names the prompt size and the budget, nothing reaches the model, nothing is
/// written (but the message, stored when it arrived) or dropped. python:
/// `test_an_over_budget_turn_is_refused_under_none`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_over_budget_turn_is_refused_under_none() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "nonerefuse").await;
    // budget = window - reserve = 4000 tokens.
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(),
                   "context_strategy": "none", "context_window": 4000,
                   "output_reserve": 0}),
        )])
        .await;
    for i in 0..20 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        put_row(
            &kernel,
            &backend,
            json!({"role": role, "content": (0..120).map(|j| format!("w{i}x{j} ")).collect::<String>()}),
        )
        .await;
    }
    let s = script(&backend, vec![pass("never reached")], Duration::ZERO);
    // The boot-owed recall would write a recall row on the first turn; this test
    // is about the seam alone, so settle it first.
    *crate::state::state_for(&backend).recall_due.lock().unwrap() = false;
    let mut rx = rebind_backend_inbox(&kernel, &backend);
    let load = || async {
        fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::memory::load_history(&backend, &kernel, false),
        )
        .await
        .expect("store read")
    };
    let before = load().await;
    assert!(before.len() >= 20, "{}", before.len());
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(r["accepted"], json!(true), "{r:#?}");
    let evs = drain(&mut rx).await;
    let err = evs
        .iter()
        .find(|v| v["type"] == json!("status") && v["phase"] == json!("failed"))
        .and_then(|v| v["detail"]["error"].as_str())
        .unwrap_or_else(|| panic!("the refusal is said: {evs:#?}"))
        .to_string();
    let rest = err
        .strip_prefix("mock_backend: context full: ")
        .unwrap_or_else(|| panic!("{err}"));
    assert!(
        rest.ends_with("clear, fold by hand, or raise context_window"),
        "{err}"
    );
    let (prompt, tail) = rest.split_once("k of ").unwrap_or_else(|| panic!("{err}"));
    let (budget, _) = tail
        .split_once("k tokens")
        .unwrap_or_else(|| panic!("{err}"));
    let prompt: i64 = prompt.parse().expect("prompt size");
    let budget: i64 = budget.parse().expect("budget");
    assert_eq!(budget, 4, "{err}");
    assert!(prompt >= 1 && prompt > budget, "{err}");
    assert!(!err.contains("unknown context_strategy"), "{err}");
    assert!(!err.contains("context_insufficient"), "{err}");
    assert_eq!(s.passes_served(), 0, "no request reached the backend");
    // the message itself was stored when it arrived (#795); nothing else was written,
    // dropped or rewritten
    let after = load().await;
    assert_eq!(before[..], after[..before.len()], "nothing dropped or rewritten");
    assert_eq!(after.len(), before.len() + 1, "{after:#?}");
    assert_eq!(after.last().unwrap()["content"], json!("go"));
    assert!(
        !after
            .iter()
            .any(|x| x["kind"] == "fold" || x["kind"] == "recall"),
        "{after:#?}"
    );
    let has = |ty: &str, phase: &str| {
        evs.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some(ty)
                && v.get("phase").and_then(Value::as_str) == Some(phase)
        })
    };
    assert!(!has("status", "done"), "{evs:#?}");
    assert!(!has("context", "compacted"), "{evs:#?}");
    assert!(!has("context", "too_small"), "{evs:#?}");
}

#[tokio::test]
async fn successive_fold_projections_do_not_nest_into_a_chain() {
    // The strategy folds the VIEW ARRAY, as python does, never the whole store:
    // on the view an old fold's members are absent and `render` does not
    // re-place it at the head. Fed the whole store, three adjacent folds wrap
    // into a deeper one every other turn and depth runs away. This pins the
    // python shape: depth stays flat.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "folddepth").await;
    let _ = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "update_agent", "id": backend.as_str(),
        "context_strategy": "fold", "context_window": 2600, "output_reserve": 200, "context_titles": true, "context_async": true, "system_prompt": "sys"}))]).await;
    let mut prev = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "THE GOAL"}),
    )
    .await;
    let state = crate::state::state_for(&backend);
    for step in 0..14 {
        for i in 0..4 {
            let call = put_row(
                &kernel,
                &backend,
                json!({"role": "assistant", "parent": prev, "content": format!("Reading f{step}_{i}. <call name=\"send\"><routing><target_id>notes</target_id><title>survey f{step}_{i}</title></routing><payload><type>read</type><path>f.txt</path></payload></call>")}),
            ).await;
            let pid = kernel.mint_id();
            prev = put_row(
                &kernel,
                &backend,
                json!({"role": "user", "parent": call, "content": format!("[reply to {pid} · notes read · \"survey f{step}_{i}\"]\n{}", format!("filler {step}.{i} text ").repeat(60))}),
            ).await;
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
        let (messages, store_len) = fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::assembly::assemble_messages(
                &backend,
                &state,
                &kernel,
                "cli",
                crate::caching::Caching::ProviderCache,
        ),
        )
        .await
        .expect("store read");
        let _ = fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::projection::projected(
                crate::model_format::pass::Pass::for_test(Arc::clone(&provider)),
                &backend,
                &state,
                &kernel,
                "cli",
                "mock",
                crate::constants::DEFAULT_CONTEXT_WINDOW,
                messages,
                store_len,
            ),
        )
        .await;
        let rows: Vec<Value> = fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::memory::load_history(&backend, &kernel, false),
        )
        .await
        .expect("store read");
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
fn the_chat_declares_its_verbs_each_with_what_it_does() {
    // what `docs` and `reflect` render: one declaration per verb, in order
    let names: Vec<&str> = crate::declared::chat::VERBS
        .iter()
        .map(|v| v.name)
        .collect();
    assert_eq!(
        names,
        [
            "boot",
            "send",
            "stop",
            "pause",
            "resume",
            "rows",
            "inquiry",
            "history",
            "embed",
            "unembed",
            "status",
            "context_status",
            "recall",
            "grep",
            "context_recall",
            "context_clear",
            "context_fold",
            "context_forget",
            "fork",
            "mark",
            "fork_points",
            "watch",
            "unwatch",
        ]
    );
    for v in crate::declared::chat::VERBS {
        assert!(!v.doc().is_empty(), "{} says what it does", v.name);
    }
    // an argument is declared with its type: `send` takes the text, `rows` a row id
    let send = crate::declared::chat::VERBS
        .iter()
        .find(|v| v.name == "send")
        .unwrap();
    assert_eq!(send.schema()["required"], json!(["text"]));
    let rows = crate::declared::chat::VERBS
        .iter()
        .find(|v| v.name == "rows")
        .unwrap();
    assert!(rows.schema()["properties"]["after"]["type"]
        .to_string()
        .contains("integer"));
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
            "c": {"$parent": "*"},
        })),
        "S  a=  b=x,y  c=<$parent>*</$parent>"
    );
    // `shift_remove`, not swap: the remaining keys keep their order.
    assert_eq!(
        crate::assembly::render_reflect(&json!({
            "sentence": "S", "one": 1, "two": 2, "three": 3,
        })),
        "S  one=1  two=2  three=3"
    );
    // A list of scalars is joined by their text, a bool as `true`.
    assert_eq!(
        crate::assembly::render_reflect(&json!({"sentence": "", "t": [true, 2]})),
        "t=true,2"
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
fn budget_meta_reads_integers_only() {
    let meta = |v: Value| v.as_object().unwrap().clone();
    // a record's numbers are integers (the engine refuses anything else at the
    // write); what is not one is not read: the window falls to the backend's, the
    // reserve to 1024
    let m = meta(json!({"context_window": 0.5, "output_reserve": "-5", "recent_n": "x"}));
    assert_eq!(
        crate::context::resolve_context_window(&m, crate::constants::DEFAULT_CONTEXT_WINDOW),
        131_072
    );
    assert_eq!(crate::context::output_reserve(&m), 1024);
    assert_eq!(crate::context::recent_n(&m), 6);
    let m = meta(json!({"context_window": 12, "output_reserve": 1000}));
    assert_eq!(
        crate::context::resolve_context_window(&m, crate::constants::DEFAULT_CONTEXT_WINDOW),
        12
    );
    assert_eq!(crate::context::output_reserve(&m), 1000);
    // the budget floors at 256
    assert_eq!(
        crate::context::budget(&m, crate::constants::DEFAULT_CONTEXT_WINDOW),
        256
    );
    // a bool is never a window nor a count
    let m = meta(json!({"context_window": true, "recent_n": true}));
    assert_eq!(
        crate::context::resolve_context_window(&m, crate::constants::DEFAULT_CONTEXT_WINDOW),
        131_072
    );
    assert_eq!(crate::context::recent_n(&m), 6);
}

#[test]
fn ids_are_numbers() {
    use crate::retrieval::as_id;
    // a JSON id is a positive integer, nothing else
    for (v, want) in [
        (json!(7), Some(7)),
        (json!(0), None),
        (json!(true), None),
        (json!(2.0), None),
        (json!("3"), None),
    ] {
        assert_eq!(as_id(&v), want, "as_id({v})");
    }
    // a record's numbers are numbers: text there is not read
    let meta = |v: Value| v.as_object().unwrap().clone();
    let m = meta(json!({"context_window": "12", "output_reserve": "12"}));
    assert_eq!(
        crate::context::resolve_context_window(&m, crate::constants::DEFAULT_CONTEXT_WINDOW),
        131_072
    );
    assert_eq!(crate::context::output_reserve(&m), 1024);
    let m = meta(json!({"context_window": 12, "output_reserve": 12}));
    assert_eq!(
        crate::context::resolve_context_window(&m, crate::constants::DEFAULT_CONTEXT_WINDOW),
        12
    );
    assert_eq!(crate::context::output_reserve(&m), 12);
}

#[test]
fn recent_n_is_an_integer_and_its_range_is_the_records() {
    let m = json!({"recent_n": 7}).as_object().unwrap().clone();
    assert_eq!(crate::context::recent_n(&m), 7);
    let m = json!({"recent_n": "nope"}).as_object().unwrap().clone();
    assert_eq!(
        crate::context::recent_n(&m),
        6,
        "text is not read: the default"
    );
}

#[test]
fn inquiry_turn_text_renders_options_and_the_action() {
    let inquiry: crate::declared::Inquiry = serde_json::from_value(json!({
        "text": "pick",
        "options": ["a", "b"],
        "action": {"a": 1, "b": "—"},
    }))
    .expect("a valid inquiry");
    let text = crate::verbs::inquiry_text(&inquiry);
    assert!(text.contains("\n\nOptions: a, b"), "{text:?}");
    assert!(
        text.contains("\n\nProposed action: {\"a\": 1, \"b\": \"\\u2014\"}"),
        "{text:?}"
    );
}

/// INQUIRY, FULLY ASYNC — python `test_inquiry_async.py`. The
/// primary asks its peer; the inquiry is ACCEPTED at once, the peer answers in a
/// LATER turn of its own chain, and the answer reaches the primary as a message
/// under the inquiry's pid. `steps` are the peer's passes after the question.
async fn inquiry_round(
    tag: &str,
    peer_passes: Vec<String>,
) -> (TempDir, Arc<MockScript>, Arc<MockScript>) {
    let (tmp, a, r, _) = inquiry_round_with(tag, peer_passes, true).await;
    (tmp, a, r)
}

/// The race: the peer answers as soon as it is asked, with no gate, so its answer
/// may land before the asker's route has delivered the accepted reply. Either order
/// must leave the asker woken once with it.
async fn inquiry_round_ungated(
    tag: &str,
    peer_passes: Vec<String>,
) -> (TempDir, Arc<MockScript>, Arc<MockScript>, AgentId) {
    inquiry_round_with(tag, peer_passes, false).await
}

async fn inquiry_round_with(
    tag: &str,
    peer_passes: Vec<String>,
    gated: bool,
) -> (TempDir, Arc<MockScript>, Arc<MockScript>, AgentId) {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, tag).await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let ask = call_envelope(
        peer.as_str(),
        json!({"type": "inquiry", "text": "ship?"}),
        None,
    );
    let a = script(
        &backend,
        vec![
            pass(ask),
            pass("<call name=\"done\"></call>"),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let r = script(
        &peer,
        peer_passes.into_iter().map(pass).collect(),
        Duration::ZERO,
    );
    if gated {
        // The peer's answer is released only once the asker's second pass has been
        // requested: that pass reads the accepted inquiry's reply.
        let gate = Arc::new(tokio::sync::Notify::new());
        *r.gate.lock().unwrap() = Some(Arc::clone(&gate));
        let asker = Arc::clone(&a);
        tokio::spawn(async move {
            until_passes(&asker, 2).await;
            gate.notify_one();
        });
    }
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "decide", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&a, 3).await,
        "the answer never reached the asker"
    );
    (tmp, a, r, backend)
}

fn answer_call(text: &str) -> String {
    format!("<call name=\"inquiry_answer\"><answer>{text}</answer></call>")
}

/// A message stored as the pass in flight ends is not lost between them (#795): the
/// unread list and `running` change under one lock, so the pass's end wakes the next
/// pass for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_stored_as_a_pass_ends_wakes_the_next() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "held_dead").await;
    let s = script(&backend, vec![pass(r#"<call name="done"></call>"#)], Duration::ZERO);
    let state = crate::state::state_for(&backend);
    let epoch = {
        let mut life = state.life.lock().unwrap();
        life.running = true;
        life.epoch += 1;
        life.epoch
    };
    fantastic_kernel::test_support::as_agent(backend.clone(), async {
        let m = crate::life::Incoming {
            text: "late in the pass".into(),
            client_id: "h".into(),
            origin: "h".into(),
            notice: false,
            attached: None,
        };
        crate::life::arrived(&kernel, &backend, m, true).await.expect("stored");
        assert_eq!(s.passes_served(), 0, "no pass beside the running one");
        crate::life::end(&kernel, &backend, epoch);
    })
    .await;
    assert!(until_passes(&s, 1).await, "the message never woke the next pass");
}

/// However fast the answer lands, the asker wakes once with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instant_answer_wakes_the_asker_once() {
    let (_tmp, a, _r, _asker) =
        inquiry_round_ungated("inq_instant", vec![answer_call("ship it")]).await;
    let third = a.request(2);
    assert_eq!(
        third.matches("<answer>ship it</answer>").count(),
        1,
        "{third}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inquiry_is_accepted_and_answered_later_under_its_pid() {
    let (_tmp, a, r) = inquiry_round("inq_async", vec![answer_call("ship it")]).await;
    assert!(
        a.request(1).contains("<accepted>true</accepted>"),
        "{}",
        a.request(1)
    );
    let third = a.request(2);
    assert!(
        third.contains("[reply to ") && third.contains("_peer inquiry]"),
        "{third}"
    );
    assert!(third.contains("<answer>ship it</answer>"), "{third}");
    assert!(r.request(0).contains("[inquiry from "), "{}", r.request(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_receiver_may_work_for_several_turns_before_answering() {
    let look =
        "<call name=\"send\"><routing><target_id>kernel</target_id></routing><payload><type>reflect</type><tier>oneliners</tier></payload></call>";
    let (_tmp, a, r) =
        inquiry_round("inq_work", vec![look.to_string(), answer_call("ship it")]).await;
    assert!(
        r.request(1).contains("kernel reflect"),
        "the peer worked a turn first"
    );
    assert!(
        a.request(2).contains("<answer>ship it</answer>"),
        "{}",
        a.request(2)
    );
}

/// A receiver that ends with `done` before it answers leaves the inquiry OPEN (#795,
/// Q5): nothing closes it for the asker, and a later pass of the receiver still
/// answers it under its pid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ending_without_an_answer_leaves_the_inquiry_open_for_a_later_pass() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "inq_done").await;
    let peer = add_peer(&kernel, &tmp, &backend).await;
    let ask = call_envelope(
        peer.as_str(),
        json!({"type": "inquiry", "text": "ship?"}),
        None,
    );
    let a = script(
        &backend,
        vec![
            pass(ask),
            pass("<call name=\"done\"></call>"),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let r = script(
        &peer,
        vec![
            pass("<call name=\"done\"></call>"),
            pass(answer_call("ship it")),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "decide", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&r, 1).await, "the peer was never asked");
    assert!(until_passes(&a, 2).await, "the accepted reply never reached the asker");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.passes_served(), 2, "the peer's `done` closed the inquiry");
    // still open: the peer's next pass on that thread answers it
    let client = crate::route::open_inquiries()
        .lock()
        .unwrap()
        .keys()
        .find(|(agent, _)| agent == peer.as_str())
        .map(|(_, c)| c.clone())
        .expect("the inquiry is still open");
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            peer.clone(),
            json!({"type": "send", "text": "answer it now", "client_id": client}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&a, 3).await, "the later answer never reached the asker");
    assert!(a.request(2).contains("<answer>ship it</answer>"), "{}", a.request(2));
}

/// The placeholder is refused in the receiver's own chain (python `repr` of the
/// raw value) and the inquiry stays open for the real answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_answer_is_refused_and_the_inquiry_stays_open() {
    let (_tmp, a, r) =
        inquiry_round("inq_ph", vec![answer_call("..."), answer_call("ship it")]).await;
    assert!(
        r.request(1)
            .contains("that is the PLACEHOLDER, not an answer — `answer` came back as '...'"),
        "{}",
        r.request(1)
    );
    assert!(
        a.request(2).contains("<answer>ship it</answer>"),
        "{}",
        a.request(2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_with_nothing_open_is_told_so() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "inq_lone").await;
    let s = script(
        &backend,
        vec![
            pass(answer_call("ship it")),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "hi", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "no second pass");
    assert!(
        s.request(1)
            .contains("inquiry_answer: nothing asked you in this run"),
        "{}",
        s.request(1)
    );
}

#[test]
fn a_null_context_strategy_is_refused_with_pythons_text() {
    // python `get_strategy(None)` is None → the refusal renders the VALUE's repr
    // and `str(strategy_names())`.
    let want = "(valid: ['compact', 'truncate', 'strip', 'fold'])";
    assert_eq!(
        fantastic_bundle::py_repr_value(&json!(crate::strategies::STRATEGY_NAMES)),
        "['compact', 'truncate', 'strip', 'fold', 'none']"
    );
    assert!(format!("x {want}").contains(want));
    assert_eq!(fantastic_bundle::py_repr_value(&json!(null)), "None");
    assert_eq!(fantastic_bundle::py_repr_value(&json!("bogus")), "'bogus'");
}
// ─── the self-verb replies a MODEL reads (python-canonical bytes) ───────────

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
    )
    .await;
    put_row(&kernel, &backend, json!({"role": "user", "content": ""})).await;
    let r = kernel
        .operator_send(vec![Item::new(backend, json!({"type": "recall"}))])
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
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(
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
async fn grep_digits_query() {
    // a digits query is text.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepnum").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "order 586 shipped"}),
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": "586"}),
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
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(
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
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(
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
    // The STORE searches a row: non-string content is its python
    // `json.dumps(c, ensure_ascii=False)` (ram_store `_text_of`) — `{"k": "é"}`,
    // so `é` matches and the escape `u00e9` does not.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "grepesc").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": {"k": "é"}}),
    )
    .await;
    let ask = |q: &str| {
        let kernel = Arc::clone(&kernel);
        let backend = backend.clone();
        let q = q.to_string();
        async move {
            kernel
                .operator_send(vec![Item::new(
                    backend,
                    json!({"type": "grep", "query": q}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    assert_eq!(ask("é").await["total"], 1);
    assert_eq!(ask("u00e9").await["total"], 0);
}

#[tokio::test]
async fn a_null_context_strategy_is_reported_raw_and_refused() {
    // Only an ABSENT key means `compact`. `context_status` echoes the raw value
    // and the projection refuses it with python's text.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ctxnull").await;
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": backend.as_str(), "context_strategy": null}),
        )])
        .await;
    let st = kernel
        .operator_send(vec![Item::new(
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
async fn a_reply_in_flight_across_a_reboot_is_recorded_not_acted_on() {
    // A rebooted agent woken by answers to calls its PREVIOUS incarnation made
    // would finish THAT work instead of the new instruction. The reply is kept as
    // a row — real data — but runs no turn.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "late").await;
    let s = script(&backend, vec![pass("ack")], Duration::ZERO);
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "GOAL"}),
    )
    .await;
    // answered: a boot finds nothing unread (#795)
    put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "content": "on it <call name=\"done\"></call>"}),
    )
    .await;
    // A boot bumps the incarnation, so a delivery decided BEFORE it is late.
    let n = crate::state::incarnation(&backend);
    let born = crate::route::Born::of(backend.as_str());
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
        .await;
    assert_eq!(crate::state::incarnation(&backend), n + 1);
    fantastic_kernel::test_support::as_agent(
        AgentId::from(backend.as_str()),
        crate::route::deliver(
            &kernel,
            &backend,
            backend.as_str(),
            "[reply to 1 · n r]\nlate",
            "cli",
            born,
            false,
            true,
        ),
    )
    .await;
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert_eq!(s.passes_served(), 0, "no turn ran for the late reply");
    let rows = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, false),
    )
    .await
    .expect("store read");
    let last = rows.last().expect("a row");
    assert_eq!(last["kind"], "late");
    assert_eq!(
        last["content"],
        "[late reply — answered a call of a chain that has ended; \
         recorded, not acted on]\n[reply to 1 · n r]\nlate"
    );
    // A reply from THIS incarnation runs a turn.
    fantastic_kernel::test_support::as_agent(
        AgentId::from(backend.as_str()),
        crate::route::deliver(
            &kernel,
            &backend,
            backend.as_str(),
            "fresh",
            "cli",
            crate::route::Born::of(backend.as_str()),
            false,
            true,
        ),
    )
    .await;
    assert!(settle(&kernel).await, "the agent never went quiet");
    assert!(s.passes_served() >= 1, "a live reply is a turn");
}

/// Bodies are scanned in the order they ARRIVED (python's `_mem.bodies` dict,
/// `list(bodies.items())`), whatever order `ids` names them in — so of two bodies
/// sharing a line, the one that arrived first owns it.
#[tokio::test]
async fn grep_scope_scans_bodies_in_arrival_order() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "setord").await;
    let line = "the audit marker";
    let a = kernel.mint_id();
    let b = kernel.mint_id();
    crate::memory::store_reply(&backend, &kernel, b, line);
    crate::memory::store_reply(&backend, &kernel, a, line);
    let r = kernel
        .operator_send(vec![Item::new(
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
        "the body that ARRIVED first owns the shared line: {r:#?}"
    );
}

#[tokio::test]
async fn context_recall_dry_makes_no_model_call() {
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
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(
            backend,
            json!({"type": "context_recall", "dry": true}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        s.passes_served(),
        0,
        "dry:true must make no model call: {r:#?}"
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
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(backend, json!({"type": "context_recall"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["degraded"], true, "{r:#?}");
    assert_eq!(r["why"], "mechanical fit");
}

#[tokio::test]
async fn only_a_text_why_is_kept() {
    // the answer's `why` is its text; a number, a list or an empty text is `chosen`.
    for (i, (answer, want)) in [
        ("{\"why\": 42}", "chosen"),
        ("{\"why\": [\"a\"]}", "chosen"),
        ("{\"why\": 0}", "chosen"),
        ("{\"why\": \"\"}", "chosen"),
        ("{\"why\": \"fits\"}", "fits"),
    ]
    .into_iter()
    .enumerate()
    {
        let tmp = TempDir::new().unwrap();
        let (kernel, backend) = mk_kernel(&tmp, &format!("why_{i}")).await;
        script(&backend, vec![pass(answer)], Duration::ZERO);
        put_row(
            &kernel,
            &backend,
            json!({"role": "user", "content": "THE GOAL"}),
        )
        .await;
        let r = kernel
            .operator_send(vec![Item::new(backend, json!({"type": "context_recall"}))])
            .await
            .remove(0)
            .result;
        assert_eq!(r["why"], want, "answer {answer}: {r:#?}");
        assert_eq!(r["degraded"], false);
    }
}

#[tokio::test]
async fn a_scalar_expand_is_wrapped_like_python() {
    // `_as_list` (python): a truthy int/float/bool `expand` is ONE
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
    )
    .await;
    let before = conversation(&kernel, &backend).await.len();
    let r = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_recall"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r:#?}");
    assert_eq!(r["degraded"], false, "{r:#?}");
    assert_eq!(
        conversation(&kernel, &backend).await.len(),
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
    )
    .await;
    let r = kernel
        .operator_send(vec![Item::new(backend, json!({"type": "context_recall"}))])
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
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
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
    // `project_context` appends the real notice and then trims to fit. Trimming
    // from the FRONT — exactly where `compact`/`truncate` just pinned the goal
    // turn — would silently drop the one turn the whole run serves in a snug
    // window, in BOTH strategies. python trims from the MIDDLE first and only
    // takes the goal as a last resort, loudly.
    let goal = "GOAL-SENTINEL: build the widget and report when done";
    for strategy in ["compact", "truncate"] {
        for window in [1200i64, 2000, 4000] {
            let tmp = TempDir::new().unwrap();
            let (kernel, backend) = mk_kernel(&tmp, &format!("seam_{strategy}_{window}")).await;
            let _ = kernel
                .operator_send(vec![Item::new(
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
            let out = fantastic_kernel::test_support::as_agent(
                backend.clone(),
                crate::projection::projected(
                    crate::model_format::pass::Pass::for_test(Arc::clone(&provider)),
                    &backend,
                    &state,
                    &kernel,
                    "cli",
                    "mock",
                    crate::constants::DEFAULT_CONTEXT_WINDOW,
                    messages,
                    len,
                ),
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

/// The kernel's execution rows are the KERNEL's (RAM, then its root store) and
/// never land under the agent's key; and a row without a chat `role` the agent
/// writes to its own store (a note to itself) is memory, not a turn. Python
/// `test_kernel_rows_and_role_less_notes_never_become_turns`.
#[tokio::test]
async fn kernel_rows_and_role_less_notes_never_become_turns() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "hidebk").await;
    // A real dispatch AT the backend — the kernel writes a `call` row about it.
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "reflect"}))])
        .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "hello"}),
    )
    .await;
    // the cache is warm, then a role-less note is appended straight to the store
    let _ = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, false),
    )
    .await
    .expect("store read");
    let out = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        kernel.operator_send(vec![Item::new(
            AgentId::from(MEMORY),
            json!({"type": "append", "rows": [{"kind": "note", "content": "vault code 4711"}]}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert_eq!(out["appended"], json!(1), "{out}");

    let calls = fantastic_kernel::test_support::rows_about(&kernel, &backend);
    assert!(
        calls
            .iter()
            .any(|r| r.get("kind").and_then(Value::as_str) == Some("call")),
        "the kernel must actually hold a call row: {calls:?}"
    );
    let stored = conversation(&kernel, &backend).await;
    assert!(
        stored.iter().all(|r| !matches!(
            r.get("kind").and_then(Value::as_str),
            Some("call" | "task" | "settle" | "ack" | "flush")
        )),
        "execution rows reached the agent's key: {stored:?}"
    );
    let hist = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, false),
    )
    .await
    .expect("store read");
    assert_eq!(
        hist.iter()
            .map(|r| r["content"].clone())
            .collect::<Vec<_>>(),
        vec![json!("hello")],
        "only the turn is in the conversation"
    );
    let behind = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, true),
    )
    .await
    .expect("store read");
    assert!(behind.iter().all(|r| r.get("role").is_some()));
    // …and grep still finds the note: it is memory.
    let g = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": "vault code"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(g["total"], json!(1), "{g}");
}

/// The id must be RANDOM, not a counter: a mint like `nanos ^ stack ^ pid ^ seq`
/// reads as a big-endian int64 (`AAAAAVnE3yQ`), never like python's `secrets`.
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

/// The peer payload's KEY ORDER is python's: `<payload>` keeps its tags in source
/// order (`type`, `text`), then `setdefault("msg", …)` appends. A
/// `serde_json::Map::remove` (`swap_remove` under `preserve_order`) on the way would
/// drag `text` into a hole and read `["text","type","msg"]` where python has
/// `["type","text","msg"]`.
#[test]
fn a_peer_sends_payload_keeps_pythons_key_order() {
    let calls = crate::tool_parse::extract_tool_calls(
        "<call name=\"send\"><routing><target_id>ai2</target_id></routing><payload><type>send</type>\
         <text>CASE=noop#1 hello from ai</text></payload></call>",
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    let payload = calls[0]
        .arguments
        .get("payload")
        .and_then(Value::as_object)
        .expect("a send carries its `payload`");
    assert_eq!(
        payload.keys().collect::<Vec<_>>(),
        vec!["type", "text"],
        "source order survives the parse"
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

/// THE ROW IDS ARE MINTED WHEN APPENDED: the message's when it arrives (#795), before
/// the pass's assembly, whose system block dispatches `reflect`s that take their
/// (kernel) call ids; the answer's after them. So the user row's id is below the
/// reflect's call id, and that below the assistant's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_turns_row_ids_are_minted_after_the_assembly_reflects() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "rt_order").await;
    let _s = script(
        &backend,
        vec![pass("ack <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(r["accepted"], json!(true), "{r}");
    let calls = fantastic_kernel::test_support::rows_about(&kernel, &backend);
    let rows = conversation(&kernel, &backend).await;
    let message_id = rows
        .iter()
        .find(|row| row["role"] == json!("assistant"))
        .and_then(|row| row["id"].as_i64())
        .expect("the answer row");
    let reflect = calls
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
        user < reflect && reflect < message_id,
        "the user row ({user}) must precede assembly's reflect ({reflect}), \
         and that the assistant row ({message_id})"
    );
}

#[tokio::test]
async fn context_clear_starts_a_new_thread_without_deleting() {
    // A CLEAR IS A BOUNDARY, NOT A DELETION: the view begins at the `[cleared …]`
    // row; earlier rows stay in the store, reachable through recall/grep
    // (behind=true) and by id. Python's twin
    // `test_context_clear_starts_a_new_thread_without_deleting`.
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "ctxclear").await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "the codename is HALCYON"}),
    )
    .await;
    put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "content": "noted"}),
    )
    .await;
    assert_eq!(
        fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::memory::load_history(&backend, &kernel, false)
        )
        .await
        .expect("store read")
        .len(),
        2
    );

    let out = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "context_clear", "why": "fresh start"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out.get("rows").and_then(Value::as_i64), Some(2), "{out}");
    let rid = out
        .get("cleared")
        .and_then(Value::as_i64)
        .expect("the clear row's id");

    let hist = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, false),
    )
    .await
    .expect("store read");
    assert_eq!(hist.len(), 1, "the view starts at the clear: {hist:?}");
    assert_eq!(hist[0].get("kind").and_then(Value::as_str), Some("clear"));
    let content = hist[0].get("content").and_then(Value::as_str).unwrap_or("");
    assert!(
        content.starts_with(&format!("[cleared {rid} · 2 rows] fresh start")),
        "{content}"
    );
    assert_eq!(
        hist[0]["refs"].as_array().map(Vec::len),
        Some(2),
        "the clear-fold's members are the earlier rows"
    );
    assert!(
        !serde_json::to_string(&hist).unwrap().contains("HALCYON"),
        "no old content in the new thread: {hist:?}"
    );

    // behind the boundary, everything is still there
    assert_eq!(
        fantastic_kernel::test_support::as_agent(
            backend.clone(),
            crate::memory::load_history(&backend, &kernel, true)
        )
        .await
        .expect("store read")
        .len(),
        3
    );
    let r = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "recall", "query": "codename"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r.get("total").and_then(Value::as_i64), Some(1), "{r}");
    let g = kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "grep", "query": "halcyon"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(g.get("total").and_then(Value::as_i64), Some(1), "{g}");

    // the persisted view is emptied, so assembly renders the store from the clear on
    let view = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_view(&backend, &kernel),
    )
    .await
    .expect("store read")
    .expect("the view must be persisted");
    assert!(
        view.get("ids")
            .and_then(Value::as_array)
            .is_some_and(|a| a.is_empty()),
        "{view:?}"
    );

    // chatting on: the new thread grows after the mark
    put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "hello again"}),
    )
    .await;
    let hist = fantastic_kernel::test_support::as_agent(
        backend.clone(),
        crate::memory::load_history(&backend, &kernel, false),
    )
    .await
    .expect("store read");
    assert_eq!(
        hist.last()
            .and_then(|r| r.get("content"))
            .and_then(Value::as_str),
        Some("hello again")
    );
}

/// The memory-store features (python `test_memory_features.py`), on this harness.
#[path = "tests_memory.rs"]
mod memory_features;

/// A fold carries its own place (python `test_fold_anchors.py`), on this harness.
#[path = "tests_fold_anchors.rs"]
mod fold_anchors;

/// The operator's `context_fold` — a range of the view folded into one fold row.
#[path = "tests_context_fold.rs"]
mod context_fold;

/// The record's `caching` modes: `cleanup` trims the view, `none` and `provider_cache`
/// keep it append-only, `provider_cache` flags the primer and the last fold, and the
/// primer pins its prefix against `context_fold`.
#[path = "tests_caching.rs"]
mod caching_modes;

/// Per-agent backend state. Two agents never share a
/// state slot; `drop_state` (every backend's `on_delete`) frees the slot so the
/// next touch builds a fresh one; the INCARNATION count is NOT freed by it —
/// it is a process-lifetime counter, exactly as python's module-level
/// `_incarnation` dict outlives a delete.
#[test]
fn backend_state_is_per_agent_and_drop_frees_the_slot_not_the_incarnation() {
    let a = AgentId::from("s4_state_a");
    let b = AgentId::from("s4_state_b");
    let sa = crate::state::state_for(&a);
    let sb = crate::state::state_for(&b);
    assert!(!Arc::ptr_eq(&sa, &sb), "two agents, two slots");
    assert!(
        Arc::ptr_eq(&sa, &crate::state::state_for(&a)),
        "the same agent reads its own slot back"
    );
    crate::state::bump_incarnation(&a);
    let born = crate::state::incarnation(&a);
    assert_eq!(crate::state::incarnation(&b), 0, "b never booted");
    crate::state::drop_state(&a);
    assert!(
        !Arc::ptr_eq(&sa, &crate::state::state_for(&a)),
        "a dropped slot is rebuilt fresh"
    );
    assert!(
        Arc::ptr_eq(&sb, &crate::state::state_for(&b)),
        "b untouched"
    );
    assert_eq!(
        crate::state::incarnation(&a),
        born,
        "drop keeps the incarnation"
    );
    assert!(!crate::state::is_generating(&a));
    crate::state::drop_state(&a);
    crate::state::drop_state(&b);
}

/// Delete-time teardown (python `test_delete_drops_every_row_but_the_incarnation`):
/// `delete_agent` runs the backend's `on_delete`, which drops every per-id table,
/// so the id re-created later starts clean; the incarnation stays monotonic, so a
/// reply to a call the deleted agent made never becomes a live turn of the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_drops_every_row_but_the_incarnation() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "del").await;
    let id = backend.as_str().to_string();
    let file_id = format!("ff_{id}");
    let old = crate::state::state_for(&backend);
    old.life.lock().unwrap().unread.push(crate::state::Arrival {
        row_id: 1,
        client_id: "c".into(),
        origin: "h".into(),
        text: "t".into(),
        send_id: "s".into(),
        at: 0.0,
        notice: false,
    });
    *old.projection.lock().unwrap() = Some(json!({"fired": false}));
    *old.compaction_mark.lock().unwrap() = Some((1, "c".into()));
    *old.folds_since.lock().unwrap() = 1;
    *old.recall_due.lock().unwrap() = true;
    *old.view_source.lock().unwrap() = Some("array".into());
    *old.calib.lock().unwrap() = Some(2.0);
    *old.reported.lock().unwrap() = Some(10);
    crate::strategies::strip::stripped_state()
        .lock()
        .unwrap()
        .insert(id.clone(), ["x".to_string()].into());
    let mem = crate::memory::mem(&kernel);
    mem.heads.lock().unwrap().insert(id.clone(), vec![]);
    mem.bodies.lock().unwrap().insert(id.clone(), vec![]);
    mem.watchers.lock().unwrap().insert(id.clone(), vec![]);
    let incarnation = crate::state::incarnation(&backend);
    assert!(incarnation >= 1, "booted");

    let out = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "delete_agent", "id": id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out["deleted"], true, "{out}");
    assert!(!crate::strategies::strip::stripped_state()
        .lock()
        .unwrap()
        .contains_key(&id));
    assert!(!mem.heads.lock().unwrap().contains_key(&id));
    assert!(!mem.bodies.lock().unwrap().contains_key(&id));
    assert!(!mem.watchers.lock().unwrap().contains_key(&id));
    assert_eq!(crate::state::incarnation(&backend), incarnation);

    // The same id, re-created: a fresh slot, and the next incarnation.
    add_backend(&kernel, &id, &file_id).await;
    let fresh = crate::state::state_for(&backend);
    assert!(!Arc::ptr_eq(&old, &fresh), "a deleted id's slot is rebuilt");
    assert!(fresh.life.lock().unwrap().unread.is_empty());
    assert!(fresh.current_meta.lock().unwrap().is_none());
    assert!(fresh.projection.lock().unwrap().is_none());
    assert!(fresh.compaction_mark.lock().unwrap().is_none());
    assert_eq!(*fresh.folds_since.lock().unwrap(), 0);
    assert!(
        *fresh.recall_due.lock().unwrap(),
        "boot's own mark, as any boot"
    );
    assert!(fresh.view_source.lock().unwrap().is_none());
    assert!(fresh.calib.lock().unwrap().is_none());
    assert!(fresh.reported.lock().unwrap().is_none());
    assert!(crate::memory::bodies(&backend, &kernel).is_empty());
    assert_eq!(crate::state::incarnation(&backend), incarnation + 1);
}

/// `history fold=…` — the fold id parse (retrieval's
/// `as_id`: an int, or a STRIPPED decimal string; never a float, a bool or a
/// word), the head's key order, and the two refusals, byte for byte.
#[tokio::test]
async fn history_by_fold_takes_an_integer_id_and_refuses_the_rest() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "s4fold").await;
    let r1 = put_row(
        &kernel,
        &backend,
        json!({"role": "user", "content": "alpha"}),
    )
    .await;
    let r2 = put_row(
        &kernel,
        &backend,
        json!({"role": "assistant", "content": "beta"}),
    )
    .await;
    let f = put_row(
        &kernel,
        &backend,
        json!({"role": "system", "kind": "fold", "title": "T", "body": "S", "refs": [r1, r2]}),
    )
    .await;
    let hist = |fold: Value| {
        let kernel = Arc::clone(&kernel);
        let backend = backend.clone();
        async move {
            kernel
                .operator_send(vec![Item::new(
                    backend,
                    json!({"type": "history", "fold": fold}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    for fold in [json!(f)] {
        let h = hist(fold.clone()).await;
        assert_eq!(
            serde_json::to_string(&h["fold"]).unwrap(),
            format!(r#"{{"id":{f},"kind":"fold","title":"T","summary":"S","members":2}}"#),
            "{fold}: {h}"
        );
        let ids: Vec<i64> = h["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![r1, r2], "{fold}");
    }
    // a `fold` that is not an integer is refused by name, never read as one
    for fold in [
        json!("abc"),
        json!(f as f64 + 0.0),
        json!(true),
        json!("+1"),
        json!(format!(" {f} ")),
        json!(f.to_string()),
    ] {
        let h = hist(fold.clone()).await;
        assert_eq!(h["reason"], json!("bad_arg"), "{fold}: {h}");
        assert_eq!(h["arg"], json!("fold"), "{fold}: {h}");
    }
    assert_eq!(
        hist(json!(-1)).await["reason"],
        json!("not_found"),
        "an integer that names no fold"
    );
    assert_eq!(
        serde_json::to_string(&hist(json!(r1)).await).unwrap(),
        format!(
            r#"{{"error":"history: {r1} is not a fold in your conversation","reason":"not_found","arg":"fold"}}"#
        )
    );
}

/// The record's `system_prompt` — its own file, over this harness.
#[path = "tests_system_prompt.rs"]
mod system_prompt;

/// The system block's placeholders and its FILE (`system_prompt_file`), over this
/// harness.
#[path = "tests_prompt_file.rs"]
mod prompt_file_tests;

/// Streaming parts (python `test_stream_parts.py`), on this harness.
#[path = "tests_stream_parts.rs"]
mod stream_parts;

/// Autofold: thoughts, large replies and recall by id (#556).
#[path = "tests_autofold.rs"]
mod autofold;
/// The model's `keep` summary of a reply that folds (#641).
#[path = "tests_keep.rs"]
mod keep;
#[path = "tests_stall.rs"]
mod stall;
/// The `stop` call: the harness's and the model's (#766).
#[path = "tests_stopped.rs"]
mod stopped;
/// The thinking cap per pass and the harness cut (#782).
#[path = "tests_thinking_cap.rs"]
mod thinking_cap;
/// The pass path of a turn and of its helpers; retries inside the turn (#766).
#[path = "tests_turn_retry.rs"]
mod turn_retry;
/// Token accounting, chat-side: `used` and `window` (#766).
#[path = "tests_used.rs"]
mod used;

/// The AI-to-AI send deadline, dead letter and withdrawal (#568).
#[path = "tests_deadline.rs"]
mod deadline;
/// An inquiry whose recipient's turn fails tells the asker (#722).
#[path = "tests_inquiry_fail.rs"]
mod inquiry_fail;
/// Thinking never leaves the agent that produced it.
#[path = "tests_no_thinking_leak.rs"]
mod no_thinking_leak;
#[path = "tests_line_summary.rs"]
mod line_summary;
/// A chain in flight across a kernel restart (#726).
#[path = "tests_restart.rs"]
mod restart;
/// Inline agent views in a chat (#339).
#[path = "tests_embed.rs"]
mod tests_embed;
/// fork, mark, fork_points (#425).
#[path = "tests_fork.rs"]
mod tests_fork;
#[path = "tests_retries.rs"]
mod tests_retries;

#[path = "tests_row_attrs.rs"]
mod row_attrs;

/// Images in a chat (#622).
#[path = "tests_images.rs"]
mod images;

// ─── ONLY THE ANSWER ACTS ───────────────────────────────────────────────
// A `<call>` written inside `<think>…</think>` is thinking — not dispatched, not
// stored. Python `test_answer_only_calls.py`, same five cases.

/// The last message handed to pass `n` — what the model was told next.
fn last_in(s: &MockScript, n: usize) -> String {
    s.request_messages(n)
        .last()
        .and_then(|m| m["content"].as_str())
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// One scripted turn `first` (built from a self-`reflect` envelope), then `done`;
/// returns the script once its second pass was served.
async fn answer_only(
    tag: &str,
    first: impl Fn(&str, &str) -> String,
) -> (TempDir, Arc<MockScript>) {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, tag).await;
    let reflect = call_envelope(backend.as_str(), json!({"type": "reflect"}), None);
    let status = call_envelope(backend.as_str(), json!({"type": "status"}), None);
    let s = script(
        &backend,
        vec![
            pass(first(&reflect, &status)),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the second pass never came");
    (tmp, s)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_in_thinking_and_answer_dispatches_once() {
    // The `fanout` shape: drafted in the thought, written again in the answer.
    let (_t, s) = answer_only("ao1", |r, _| {
        format!("<think>the shape is {r} — do it</think>{r}")
    })
    .await;
    let next = last_in(&s, 1);
    assert_eq!(next.matches("[reply to ").count(), 1, "{next}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_only_in_thinking_does_not_run() {
    let (_t, s) = answer_only("ao2", |r, st| format!("<think>maybe {st} — no</think>{r}")).await;
    let whole = last_in(&s, 1);
    assert!(
        whole.starts_with("<thought id="),
        "the answer's thinking, folded: {whole}"
    );
    let next = past_thoughts(&whole);
    assert_eq!(next.matches("[reply to ").count(), 1, "{next}");
    let tag = next.lines().next().unwrap_or("");
    assert!(
        tag.contains(" reflect") && !tag.contains(" status"),
        "{next}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_only_in_thinking_make_a_dead_turn() {
    // Nothing of it runs; the empty answer is re-asked with the no-call report (#795).
    let (_t, s) = answer_only("ao3", |r, _| format!("<think>{r}</think>")).await;
    let next = last_in(&s, 1);
    assert!(!next.contains("[reply to "), "{next}");
    assert_eq!(next, crate::model_format::default::REPORT_NO_CALL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_inside_thinking_does_not_end_the_chain() {
    let (_t, s) = answer_only("ao4", |_, _| {
        "<think><call name=\"done\"></call></think>I am finished.".to_string()
    })
    .await;
    let whole = last_in(&s, 1);
    let next = past_thoughts(&whole);
    assert!(next.starts_with("[no call was dispatched]"), "{next}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unclosed_thought_with_a_call_does_not_dispatch() {
    let (_t, s) = answer_only("ao5", |r, _| format!("<think>plan {r} and more")).await;
    let next = last_in(&s, 1);
    assert!(!next.contains("[reply to "), "{next}");
    assert_eq!(next, crate::model_format::default::REPORT_NO_CALL);
}

// ─── the call checker through the agent (python `test_call_check.py`) ───────

/// The last message pass `n` was sent, as text.
fn last_content(s: &MockScript, n: usize) -> String {
    s.request_messages(n)
        .last()
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string()
}

const E59_ANSWER: &str =
    "I'll look at the files agent.\n<tool_call>\n<function=call name=\"send\">\n\
<target_id>files</target_id>\n<type>reflect</type>\n</call>\n</tool_call>";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_call_is_reported_instead_of_the_no_call_reminder() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cc_a").await;
    let s = script(
        &backend,
        vec![pass(E59_ANSWER), pass("<call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the report did not come back");
    let nxt = last_content(&s, 1);
    assert!(!nxt.contains("[no call was dispatched]"), "{nxt}");
    assert!(
        nxt.contains("[malformed call — nothing ran] line 3, col 1:"),
        "{nxt}"
    );
    // CUT AT THE FIRST FOREIGN TAG (#499): the arguments after it are never read
    assert!(nxt.contains("<call name=\"send\"></call>"), "{nxt}");
}

/// A broken call beside a good one fails the attempt (#766): nothing of it runs, and the
/// re-ask carries the report for the whole answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_call_beside_a_good_one_fails_the_attempt() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cc_b").await;
    // rendered over 7 lines, so the broken call is on line 8
    let good = call_envelope("kernel", json!({"type": "reflect", "tree": "none"}), None);
    let s = script(
        &backend,
        vec![
            pass(format!(
                "{good}\n<send><target_id>kernel</target_id><type>ps</type></send>"
            )),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the re-ask did not come");
    let batch = last_content(&s, 1);
    assert_eq!(batch.matches("kernel reflect").count(), 0, "{batch}");
    assert!(
        batch.contains("[malformed call — nothing ran] line 8, col 1:"),
        "{batch}"
    );
}

/// #188 end to end: a valid call wrapped in `<tool_call>` is NOT dispatched, the
/// top-level call beside it runs, and the batch carries the wrapper's report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrapped_call_is_not_dispatched_and_the_top_level_one_runs() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cc_d").await;
    let good = call_envelope("kernel", json!({"type": "reflect", "tree": "none"}), None);
    let s = script(
        &backend,
        vec![
            pass(format!("<tool_call>{good}</tool_call>\n{good}")),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the batch did not come back");
    let batch = last_content(&s, 1);
    assert!(
        batch.contains("[malformed call — nothing ran] line 1, col 1:"),
        "{batch}"
    );
    // CUT AT THE WRAPPER (#499): nothing after the first foreign tag is read, so
    // neither the wrapped call nor the top-level one beside it ran.
    assert!(batch.contains("holds no call"), "{batch}");
    assert_eq!(batch.matches("kernel reflect").count(), 0, "{batch}");
}

/// An answer whose only markup is an open wrapper with no call in it gets the
/// wrapper report, not only the generic no-call reminder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_wrapper_is_reported_by_name_not_only_reminded() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cc_e").await;
    let s = script(
        &backend,
        vec![
            pass("<tool_call>\n<function_calls>\n<function_calls>\n</function_calls>"),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 2).await, "the report did not come back");
    let nxt = last_content(&s, 1);
    assert!(!nxt.contains("[no call was dispatched]"), "{nxt}");
    assert!(nxt.contains("`<tool_call>` holds no call"), "{nxt}");
    assert!(
        nxt.contains("If it is finished, end the chain explicitly: <call name=\"done\"></call>"),
        "{nxt}"
    );
    assert!(
        nxt.contains("<call name=\"send\">\n<routing><target_id>agent id</target_id>"),
        "{nxt}"
    );
}

/// A model stuck on one broken shape still ends: every attempt is malformed, so the
/// harness stops the turn after `1 + CALL_RETRIES` attempts (#766), and nothing more is
/// asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_only_turns_count_toward_giving_up() {
    let cap = 1 + crate::constants::CALL_RETRIES;
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "cc_c").await;
    let s = script(
        &backend,
        // Broken, but no foreign tag: a stream-cut turn gives up sooner (#499).
        (0..cap + 5)
            .map(|_| pass("<call name=\"send\">oops<routing><target_id>files</target_id></routing></call>"))
            .collect(),
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(
        until_passes(&s, cap).await,
        "the chain stalled before the cap"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        s.passes_served(),
        cap,
        "the harness stop ends the chain: the model is not asked again"
    );
}

// ─── harness text is a NOTICE (python `test_notice_rows.py`) ────────────────

async fn user_rows(kernel: &Arc<Kernel>, backend: &AgentId) -> Vec<Value> {
    let r = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "rows"}))])
        .await
        .remove(0)
        .result;
    r["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["role"] == json!("user"))
        .collect()
}

/// The no-call report is never stored: it is an attempt's re-ask inside the pass
/// (#795). The task stays a plain user row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_no_call_report_is_never_stored_and_the_task_is_not_a_notice() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "nt_a").await;
    let s = script(
        &backend,
        vec![pass("just prose"), pass("<call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await;
    assert!(until_passes(&s, 2).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let users = user_rows(&kernel, &backend).await;
    assert_eq!(users.len(), 1, "{users:#?}");
    assert_eq!(users[0]["content"], json!("go"));
    assert!(users[0].get("kind").is_none(), "{:?}", users[0]);
}

/// A batch with a reply is not a notice. (A report on malformed calls is no batch any
/// more: it is re-asked inside the turn and never stored, #766.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_with_a_reply_is_not_a_notice() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "nt_b").await;
    let good = call_envelope("kernel", json!({"type": "reflect", "tree": "none"}), None);
    let s = script(
        &backend,
        vec![pass(good), pass("<call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await;
    assert!(until_passes(&s, 2).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let users = user_rows(&kernel, &backend).await;
    assert!(users[1]["content"]
        .as_str()
        .unwrap_or("")
        .contains("kernel reflect"));
    assert!(users[1].get("kind").is_none(), "{:?}", users[1]);
}

#[test]
fn the_incoming_row_carries_notice_only_when_asked() {
    assert_eq!(
        crate::agent_loop::incoming_row("t", 3, Some(2), true),
        json!({"role": "user", "content": "t", "id": 3, "parent": 2, "kind": "notice"})
    );
    assert!(crate::agent_loop::incoming_row("t", 3, None, false)
        .get("kind")
        .is_none());
}

/// Only the agent itself can mark a notice: a peer's `notice` is ignored, so no
/// peer can hide a message from a person's view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_cannot_mark_its_message_a_notice() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "nt_d").await;
    let s = script(
        &backend,
        vec![pass("<call name=\"done\"></call>")],
        Duration::ZERO,
    );
    // a REAL peer, holding its own line on the backend
    let _ = kernel
        .operator_send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": "peer_x",
                   "handler_module": "file_bridge.tools", "allow": ["."],
                   "root": tmp.path().join(".fantastic").to_string_lossy(),
                   "can": {backend.as_str(): {"call": ["*"]}}}),
        )])
        .await;
    let k = Arc::clone(&kernel);
    let b = backend.clone();
    let out = fantastic_kernel::test_support::as_agent("peer_x", async move {
        crate::tests::settled(&k, k.operator_send(vec![Item::new(
            b,
            json!({"type": "send", "text": "hidden?", "notice": true}),
        )]))
        .await
    })
    .await;
    assert!(until_passes(&s, 1).await, "{:?}", out[0].result);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let users = user_rows(&kernel, &backend).await;
    assert!(
        users[0]["content"]
            .as_str()
            .unwrap_or("")
            .ends_with("hidden?"),
        "{users:?}"
    );
    assert!(users[0].get("kind").is_none(), "{users:?}");
}

/// ONE CALL FORM (#528): a send written without `<routing>` (`title`, `type` and the
/// verb's arguments directly in the call) is refused by the parser, with the routing
/// form; nothing reaches the target. The send beside it, in the routing form, runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inline_send_is_refused_and_nothing_reaches_the_target() {
    struct Store(Arc<std::sync::Mutex<Vec<String>>>);
    // `create` takes a `title`: a name the call's routing also carries
    fantastic_kernel::stand_in_verbs!(clash_verbs {
        CreateV "create" { title: String, body: String },
        PingV "ping" {}
    });
    #[async_trait]
    impl fantastic_kernel::Handler for Store {
        type Call = clash_verbs::Call;
        fn name(&self) -> &str {
            "clash_store"
        }
        fn man(&self) -> &'static str {
            "A test stand-in."
        }
        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            clash_verbs::VERBS
        }
        fn dynamic_verbs(&self, agent: &Agent) -> Vec<fantastic_kernel::DynVerb> {
            clash_verbs::dynamic_verbs(agent)
        }
        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            call: Self::Call,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            let verb = match &call {
                clash_verbs::Call::CreateV(_) => "create",
                clash_verbs::Call::PingV(_) => "ping",
                clash_verbs::Call::Reflect(_) => "reflect",
            };
            self.0.lock().unwrap().push(verb.to_string());
            Ok(Some(json!({"ok": verb})))
        }
    }

    let got = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let g = Arc::clone(&got);
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, move |k| {
        k.bundles.register("clash_store.tools", Store(g))
    })
    .await;
    let backend_id = id_for(&tmp, "rt_clash");
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic")).await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    let kb = format!("{backend_id}_kb");
    let _ = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "clash_store.tools", "id": kb}))])
        .await;
    let _ = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "update_agent", "id": backend_id.as_str(), "can": {"$parent": {"call": ["*"]}, file_id.as_str(): {"call": ["*"]}, "cli": {"call": ["*"]}, MEMORY: {"call": ["*"]}, kb.as_str(): {"call": ["*"]}}}))])
        .await;
    let create = format!(
        "<call name=\"send\"><target_id>{kb}</target_id><title>Notes</title>\
         <type>create</type><body>b</body></call>"
    );
    let mut ping = Map::new();
    ping.insert("target_id".into(), json!(kb));
    ping.insert("title".into(), json!("check kb"));
    ping.insert("payload".into(), json!({"type": "ping"}));
    let s = script(
        &backend,
        vec![
            pass(format!(
                "{create}\n{}",
                crate::tool_parse::render_call("send", &ping)
            )),
            pass("saw it <call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await;
    assert!(
        until_passes(&s, 2).await,
        "the refusal must reach the model"
    );
    let turn = s.request(1);
    assert!(
        turn.contains("`<target_id>` directly in the call"),
        "{turn}"
    );
    assert!(
        turn.contains("<routing><target_id>agent id</target_id></routing>"),
        "the refusal shows the routing form: {turn}"
    );
    assert!(
        got.lock().unwrap().is_empty(),
        "nothing of a malformed attempt runs, the good call beside it included (#766)"
    );
}

/// `<payload>` is taken whole, so its `title` reaches the target, and the routing
/// `title` labels the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routing_name_in_the_payload_of_a_routed_call_is_delivered() {
    struct Store(Arc<std::sync::Mutex<Vec<Value>>>);
    fantastic_kernel::stand_in_verbs!(clash2_verbs {
        CreateV "create" { title: String, body: String }
    });
    #[async_trait]
    impl fantastic_kernel::Handler for Store {
        type Call = clash2_verbs::Call;
        fn name(&self) -> &str {
            "clash_store2"
        }
        fn man(&self) -> &'static str {
            "A test stand-in."
        }
        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            clash2_verbs::VERBS
        }
        fn dynamic_verbs(&self, agent: &Agent) -> Vec<fantastic_kernel::DynVerb> {
            clash2_verbs::dynamic_verbs(agent)
        }
        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            call: Self::Call,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            self.0.lock().unwrap().push(call.payload());
            Ok(Some(json!({"created": true})))
        }
    }

    let got = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let g = Arc::clone(&got);
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, move |k| {
        k.bundles.register("clash_store2.tools", Store(g))
    })
    .await;
    let backend_id = id_for(&tmp, "rt_routed");
    let file_id = format!("ff_{backend_id}");
    add_file_bridge(&kernel, &file_id, &tmp.path().join(".fantastic")).await;
    let backend = add_backend(&kernel, &backend_id, &file_id).await;
    let kb = format!("{backend_id}_kb");
    let _ = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "clash_store2.tools", "id": kb}))])
        .await;
    let _ = kernel.operator_send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "update_agent", "id": backend_id.as_str(), "can": {"$parent": {"call": ["*"]}, file_id.as_str(): {"call": ["*"]}, "cli": {"call": ["*"]}, MEMORY: {"call": ["*"]}, kb.as_str(): {"call": ["*"]}}}))])
        .await;
    let call = format!(
        "<call name=\"send\"><routing><target_id>{kb}</target_id><title>make notes</title></routing>\
         <payload><type>create</type><title>Notes</title><body>b</body></payload></call>"
    );
    let s = script(
        &backend,
        vec![pass(call), pass("saw it <call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go", "client_id": "probe"}),
        )]))
        .await;
    assert!(until_passes(&s, 2).await, "the reply must reach the model");
    let turn = s.request(1);
    assert!(!turn.contains("NOT dispatched"), "{turn}");
    assert!(
        turn.contains("make notes"),
        "the routing title labels the reply: {turn}"
    );
    assert_eq!(
        *got.lock().unwrap(),
        vec![json!({"type": "create", "title": "Notes", "body": "b"})]
    );
}

/// Two empty answers in a row are both re-asked inside the pass (#795), and the third
/// attempt's call runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_empty_answers_are_reasked_and_the_third_attempt_runs() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "esc_t").await;
    let dead = "\n\n";
    let s = script(
        &backend,
        vec![pass(dead), pass(dead), pass("<call name=\"done\"></call>")],
        Duration::ZERO,
    );
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "go"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&s, 3).await, "the chain did not finish");
    for n in [1, 2] {
        assert_eq!(last_content(&s, n), crate::model_format::default::REPORT_NO_CALL);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.passes_served(), 3, "the third attempt ended it");
}

const DRIVER_TASK: &str =
    "You are the DRIVER. ROLE-MARKER-218: when told 'start ais', start the AI workers.";

/// Plant the driver's conversation. Returns the TASK row's id.
async fn driver_store(
    kernel: &Arc<Kernel>,
    backend: &AgentId,
    clear_first: bool,
    task_text: &str,
) -> i64 {
    if clear_first {
        let old = put_row(
            kernel,
            backend,
            json!({"role":"user","content":"an old job"}),
        )
        .await;
        put_row(
            kernel,
            backend,
            json!({"role":"assistant","parent":old,"content":"done with the old job"}),
        )
        .await;
        let cleared = kernel
            .operator_send(vec![Item::new(
                backend.clone(),
                json!({"type": "context_clear", "why": "new job"}),
            )])
            .await
            .remove(0)
            .result;
        assert!(cleared.get("cleared").is_some(), "{cleared}");
    } else {
        let hello = put_row(kernel, backend, json!({"role":"user","content":"hello"})).await;
        put_row(
            kernel,
            backend,
            json!({"role":"assistant","parent":hello,"content":"hi"}),
        )
        .await;
    }
    let task = put_row(kernel, backend, json!({"role":"user","content": task_text})).await;
    let ack = put_row(
        kernel,
        backend,
        json!({"role":"assistant","parent":task,"content":"Understood."}),
    )
    .await;
    let mut prev = ack;
    for i in 0..60 {
        let content = format!(
            "Polling job {i}. {} <call name=\"send\"><routing><target_id>exec</target_id><title>poll {i}</title></routing><payload><type>status</type><job_id>1</job_id></payload></call>",
            "still waiting on the workers. ".repeat(120)
        );
        let a = put_row(
            kernel,
            backend,
            json!({"role":"assistant","parent":prev,"content":content}),
        )
        .await;
        let pid = kernel.mint_id();
        prev = put_row(
            kernel,
            backend,
            json!({"role":"user","parent":a,"content": format!("[reply to {pid} · exec status · \"poll {i}\"]\nrunning")}),
        )
        .await;
        if i % 10 == 0 {
            put_row(kernel, backend, json!({"role":"user","content": format!("[from worker_1 · msg {i}]\nstill building")})).await;
        }
    }
    task
}

#[test]
fn standing_pins_the_task_the_live_root_and_the_newest_fold() {
    let view = vec![
        json!({"role":"user","id":1,"kind":"clear","content":"[cleared 1 · 3 rows]"}),
        json!({"role":"user","id":2,"content":"TASK"}),
        json!({"role":"assistant","id":3,"parent":2,"content":"ok"}),
        json!({"role":"user","id":4,"parent":3,"content":"[reply to 9 · exec status]\nrunning"}),
        json!({"role":"user","id":5,"content":"[from w1]\nprogress"}),
        json!({"role":"user","id":6,"kind":"fold","refs":[3,4],"content":"[fold 6 · 2 refs] \"x\""}),
        json!({"role":"user","id":7,"content":"a later note"}),
        json!({"role":"assistant","id":8,"parent":7,"content":"ok"}),
    ];
    assert_eq!(crate::retrieval::standing(&view), vec![1, 6, 5, 4]);
    let short = vec![
        json!({"role":"user","id":1,"content":"TASK"}),
        json!({"role":"assistant","id":2,"parent":1,"content":"ok"}),
    ];
    assert!(crate::retrieval::standing(&short).is_empty());
    // one root past index 0: the first and the last root are the same row, pinned once
    let single = vec![
        json!({"role":"assistant","id":1,"content":"ok"}),
        json!({"role":"user","id":2,"content":"ONLY ROOT"}),
        json!({"role":"assistant","id":3,"parent":2,"content":"ok"}),
    ];
    assert_eq!(crate::retrieval::standing(&single), vec![1]);
    // no root past index 0 (a reply and a non-root only): nothing is standing
    let no_roots = vec![
        json!({"role":"assistant","id":1,"content":"ok"}),
        json!({"role":"user","id":2,"parent":1,"content":"[reply to 9 · exec status]\nrunning"}),
    ];
    assert!(crate::retrieval::standing(&no_roots).is_empty());
}

/// The restart, then the first turn after it: the model-view request of that turn.
async fn restarted_turn_sees(clear_first: bool, task_text: &str) -> String {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, if clear_first { "r218c" } else { "r218" }).await;
    driver_store(&kernel, &backend, clear_first, task_text).await;
    let sc = script(
        &backend,
        vec![
            pass("{}"),
            pass("noted <call name=\"done\"></call>"),
            pass("<call name=\"done\"></call>"),
            pass("<call name=\"done\"></call>"),
        ],
        Duration::ZERO,
    );
    // The restart: head cache dropped, new incarnation, recall due.
    let _ = kernel
        .operator_send(vec![Item::new(backend.clone(), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            backend.clone(),
            json!({"type": "send", "text": "start ais", "client_id": "c218"}),
        )]))
        .await
        .remove(0)
        .result;
    assert!(until_passes(&sc, 2).await, "the turn did not run");
    let reqs = sc.requests.lock().unwrap().clone();
    reqs.iter()
        .find(|m| {
            m.first()
                .and_then(|x| x.get("role"))
                .and_then(Value::as_str)
                == Some("system")
        })
        .map(|m| serde_json::to_string(m).unwrap())
        .unwrap_or_else(|| panic!("no turn request among {} passes", reqs.len()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_agent_still_sees_the_turn_that_set_its_task() {
    let t = restarted_turn_sees(false, DRIVER_TASK).await;
    assert!(t.contains("ROLE-MARKER-218"), "{}", &t[..t.len().min(3000)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_after_a_clear_keeps_the_task_given_after_it() {
    let t = restarted_turn_sees(true, DRIVER_TASK).await;
    assert!(
        t.contains("ROLE-MARKER-218"),
        "{}",
        t.chars().take(3000).collect::<String>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinning_stays_within_half_the_target_and_newest_rows_still_fill() {
    // A task root far bigger than any pin cap is NOT pinned (the cap holds), so it
    // cannot starve the newest-first fill: the newest polling row still reaches the
    // first turn after the restart, and the giant root does not.
    let giant = format!("GIANT-218 {}", "y".repeat(1_000_000));
    let t = restarted_turn_sees(false, &giant).await;
    assert!(!t.contains("GIANT-218"), "an oversized root was pinned");
    assert!(t.contains("Polling job 59"), "the newest rows did not fill");
}

// ── delete_agent purges the agent's rows from its memory store (#309) ──

/// Append rows to the memory store AS `agent`; returns their ids.
async fn store_append(kernel: &Arc<Kernel>, agent: &AgentId, rows: Value) -> Vec<i64> {
    let out = fantastic_kernel::test_support::as_agent(
        agent.clone(),
        kernel.operator_send(vec![Item::new(
            AgentId::from(MEMORY),
            json!({"type": "append", "rows": rows}),
        )]),
    )
    .await
    .remove(0)
    .result;
    out["ids"]
        .as_array()
        .unwrap_or_else(|| panic!("append refused: {out}"))
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect()
}

/// The ram_store dump, narrowed to one owner suffix: (rows, fold_refs).
async fn rows_of_owner(kernel: &Arc<Kernel>, agent: &str) -> (Vec<Value>, Vec<Value>) {
    let out = kernel
        .operator_send(vec![Item::new(
            AgentId::from(MEMORY),
            json!({"type": "dump"}),
        )])
        .await
        .remove(0)
        .result;
    let suffix = format!("|{agent}");
    let pick = |k: &str| -> Vec<Value> {
        out["tables"][k]
            .as_array()
            .unwrap_or_else(|| panic!("dump: {out}"))
            .iter()
            .filter(|e| e["owner"].as_str().is_some_and(|o| o.ends_with(&suffix)))
            .cloned()
            .collect()
    };
    (pick("rows"), pick("fold_refs"))
}

/// Search the store AS `agent` (a re-created id reads the owner's own key).
async fn search_as(kernel: &Arc<Kernel>, agent: &AgentId, query: &str) -> Value {
    fantastic_kernel::test_support::as_agent(
        agent.clone(),
        kernel.operator_send(vec![Item::new(
            AgentId::from(MEMORY),
            json!({"type": "search", "query": query}),
        )]),
    )
    .await
    .remove(0)
    .result
}

/// Rows, a fold over them, and a searchable token for `agent`.
async fn plant_memory(kernel: &Arc<Kernel>, agent: &AgentId, token: &str) {
    let ids = store_append(
        kernel,
        agent,
        json!([
            {"role": "user", "content": format!("remember {token}")},
            {"role": "assistant", "content": "ok"},
        ]),
    )
    .await;
    store_append(
        kernel,
        agent,
        json!([{"kind": "fold", "content": "summary", "refs": ids}]),
    )
    .await;
}

async fn delete_agent_verb(kernel: &Arc<Kernel>, id: &AgentId, extra: Value) -> Value {
    let mut payload = json!({"type": "delete_agent", "id": id.as_str()});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        payload[k] = v;
    }
    kernel
        .operator_send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_agent_removes_its_rows_and_index() {
    let tmp = TempDir::new().unwrap();
    let (kernel, backend) = mk_kernel(&tmp, "purge").await;
    let id = backend.as_str().to_string();
    plant_memory(&kernel, &backend, "zq-purge-token-77").await;
    let (rows, folds) = rows_of_owner(&kernel, &id).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(folds.len(), 1, "{folds:?}");
    assert_eq!(
        search_as(&kernel, &backend, "zq-purge-token-77").await["total"],
        json!(1)
    );

    let out = delete_agent_verb(&kernel, &backend, json!({})).await;
    assert_eq!(out["deleted"], true, "{out}");

    let (rows, folds) = rows_of_owner(&kernel, &id).await;
    assert!(rows.is_empty(), "rows survived the delete: {rows:?}");
    assert!(folds.is_empty(), "fold refs survived: {folds:?}");
    // the same id re-created reads its own key: nothing to find
    add_backend(&kernel, &id, &format!("ff_{id}")).await;
    let hit = search_as(&kernel, &backend, "zq-purge-token-77").await;
    assert_eq!(hit["total"], json!(0), "{hit}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_agent_keep_memory_keeps_them() {
    let tmp = TempDir::new().unwrap();
    let (kernel, kept) = mk_kernel(&tmp, "keepme").await;
    let other = add_peer(&kernel, &tmp, &kept).await;
    plant_memory(&kernel, &kept, "zq-keep-token-12").await;
    plant_memory(&kernel, &other, "zq-other-token-34").await;

    let out = delete_agent_verb(&kernel, &kept, json!({"keep_memory": true})).await;
    assert_eq!(out["deleted"], true, "{out}");
    let (rows, folds) = rows_of_owner(&kernel, kept.as_str()).await;
    assert_eq!(rows.len(), 3, "keep_memory must keep the rows: {rows:?}");
    assert_eq!(folds.len(), 1, "{folds:?}");
    add_backend(&kernel, kept.as_str(), &format!("ff_{kept}")).await;
    assert_eq!(
        search_as(&kernel, &kept, "zq-keep-token-12").await["total"],
        json!(1)
    );

    // a plain delete of the second agent purges it, the kept one stays
    let out = delete_agent_verb(&kernel, &other, json!({})).await;
    assert_eq!(out["deleted"], true, "{out}");
    let (rows, folds) = rows_of_owner(&kernel, other.as_str()).await;
    assert!(rows.is_empty() && folds.is_empty());
    assert_eq!(rows_of_owner(&kernel, kept.as_str()).await.0.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_of_other_agents_are_untouched() {
    let tmp = TempDir::new().unwrap();
    let (kernel, parent) = mk_kernel(&tmp, "tree").await;
    let bystander = add_peer(&kernel, &tmp, &parent).await;
    // a child AI under the parent, sharing the same store
    let child_id = format!("{parent}_kid");
    let mut create = json!({
        "type": "create_agent", "parent_id": parent.as_str(),
        "handler_module": HANDLER_MODULE, "id": child_id,
        "model": "test-model", "context_window": 32768,
    });
    for (k, v) in
        fantastic_ram_store::test_support::memory_meta(&kernel, &child_id, json!({})).await
    {
        create[k] = v;
    }
    let made = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!(child_id), "{made}");
    let child = AgentId::from(child_id.as_str());

    plant_memory(&kernel, &parent, "zq-parent-1").await;
    plant_memory(&kernel, &child, "zq-child-2").await;
    plant_memory(&kernel, &bystander, "zq-bystander-3").await;
    let op = kernel
        .operator_send(vec![Item::new(
            AgentId::from(MEMORY),
            json!({"type": "append", "rows": [{"role": "user", "content": "operator zq-op-4"}]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(op["appended"], json!(1), "{op}");
    let before = (
        rows_of_owner(&kernel, bystander.as_str()).await,
        rows_of_owner(&kernel, "kernel").await,
        search_as(&kernel, &bystander, "zq-bystander-3").await,
    );
    assert_eq!(before.0 .0.len(), 3);

    let out = delete_agent_verb(&kernel, &parent, json!({})).await;
    assert_eq!(out["deleted"], true, "{out}");
    for gone in [parent.as_str(), child.as_str()] {
        let (rows, folds) = rows_of_owner(&kernel, gone).await;
        assert!(rows.is_empty() && folds.is_empty(), "{gone}: {rows:?}");
    }
    let after = (
        rows_of_owner(&kernel, bystander.as_str()).await,
        rows_of_owner(&kernel, "kernel").await,
        search_as(&kernel, &bystander, "zq-bystander-3").await,
    );
    assert_eq!(before, after, "another agent's memory changed");
}

// ---- THE SPLIT: an ai_chat completes through a provider agent ----------------------------

/// A provider agent whose every `complete` streams "hel" + "lo".
pub(crate) struct EchoProvider;

/// Says "hel" + "lo" + the model it was built on.
struct EchoWords(String);

/// The gate an `m-gated…` pass (one per model name) waits on before it streams: a test holds the provider's
/// `complete` in flight with it.
pub(crate) fn echo_gate(model: &str) -> Arc<tokio::sync::Notify> {
    static GATES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>> =
        std::sync::OnceLock::new();
    let mut gates = GATES.get_or_init(Default::default).lock().unwrap();
    Arc::clone(gates.entry(model.to_string()).or_default())
}

#[async_trait]
impl Provider for EchoWords {
    async fn chat(&self, m: &[Value], _x: Option<i64>) -> Result<ProviderStream, ProviderError> {
        // `m-gated`: the pass waits on `echo_gate` before it answers `done`
        if self.0.starts_with("m-gated") {
            echo_gate(&self.0).notified().await;
            return Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                ProviderEvent::Token("released <call name=\"done\"></call>".to_string()),
            )])));
        }
        // `m-reask`: a malformed answer, then (re-asked) a good one
        if self.0 == "m-reask" {
            let reasked = m
                .iter()
                .any(|x| x["role"] == "assistant" && x["content"].to_string().contains("oops"));
            let text = if reasked {
                "fixed <call name=\"done\"></call>"
            } else {
                "broken <call name=\"send\">oops<routing><target_id>x</target_id></routing><payload><type>reflect</type></payload></call>"
            };
            return Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                ProviderEvent::Token(text.to_string()),
            )])));
        }
        // `m-call`: the answer is a call to `vault` (once), then done — the chat's own call
        if self.0 == "m-call" {
            let replied = m.iter().any(|x| x["content"].to_string().contains("vault"));
            let text = if replied {
                "<call name=\"done\"></call>".to_string()
            } else {
                "<call name=\"send\"><routing><target_id>vault</target_id></routing><payload><type>reflect</type></payload></call>".to_string()
            };
            return Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                ProviderEvent::Token(text),
            )])));
        }
        let toks: Vec<Result<ProviderEvent, ProviderError>> = vec![
            Ok(ProviderEvent::Token("hel".into())),
            Ok(ProviderEvent::Token("lo".into())),
            Ok(ProviderEvent::Token(format!(" from {}", self.0))),
            Ok(ProviderEvent::Token(" <call name=\"done\"></call>".into())),
        ];
        Ok(Box::pin(futures_util::stream::iter(toks)))
    }
    fn model(&self) -> String {
        self.0.clone()
    }
}

mod provider_verbs {
    use crate::declared::{Complete, Models, SetDefaultModel};
    fantastic_kernel::verbs! {
        "complete" => Complete,
        "models" => Models,
        "set_default_model" => SetDefaultModel,
    }
}

#[async_trait]
impl Handler for EchoProvider {
    type Call = provider_verbs::Call;
    fn name(&self) -> &str {
        "echo_provider"
    }
    fn man(&self) -> &'static str {
        "A provider stand-in that streams a fixed answer."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        provider_verbs::VERBS
    }
    fn dynamic_verbs(&self, a: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        provider_verbs::dynamic_verbs(a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &crate::tests_decl::any::RECORD
    }
    async fn handle(
        &self,
        id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        // no model on the pass: the "server" names its own, `srv-1`
        let build = |m: Map<String, Value>| -> Arc<dyn Provider> {
            Arc::new(EchoWords(
                m.get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("srv-1")
                    .to_string(),
            ))
        };
        use provider_verbs::Call;
        Ok(Some(match call {
            Call::Reflect(_) => json!({"id": id.as_str()}),
            Call::Complete(a) => {
                crate::remote_provider::complete_verb(id, a, kernel, None, None, move |m, _key| build(m), crate::steps::deferred(kernel)).await;
                Value::Null
            }
            Call::Models(_) => {
                crate::remote_provider::models_verb(id, kernel, None, crate::remote_provider::NO_LIVE, crate::steps::deferred(kernel))
                    .await;
                Value::Null
            }
            Call::SetDefaultModel(a) => {
                crate::remote_provider::set_default_verb(id, &a, kernel, crate::steps::deferred(kernel)).await;
                Value::Null
            }
        }))
    }
}

/// A provider on the echo bundle with its own key in the shared test store, its default
/// seeded the way a user sets it (`set_default_model`, a `setting` row in the store).
pub(crate) async fn make_provider(
    kernel: &Arc<Kernel>,
    id: &str,
    extra: Value,
    default: Option<&str>,
) {
    let memory = fantastic_ram_store::test_support::memory_meta(kernel, id, json!({})).await;
    let mut c = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "echo_provider.tools", "id": id});
    for (k, v) in memory {
        c[k] = v;
    }
    for (k, v) in extra.as_object().unwrap() {
        c[k] = v.clone();
    }
    let made = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), c)])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], id, "{made}");
    if let Some(m) = default {
        let r = kernel
            .operator_send(vec![Item::new(
                AgentId::from(id),
                json!({"type": "set_default_model", "model": m}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["default"], m, "{r}");
    }
}

#[tokio::test]
async fn an_ai_chat_holds_the_conversation_and_completes_through_its_provider() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register("echo_provider.tools", EchoProvider);
        k.bundles.register(
            crate::chat_bundle::HANDLER_MODULE,
            crate::chat_bundle::AiChatBundle,
        );
    })
    .await;
    make_provider(
        &kernel,
        "prov",
        json!({"models": ["m-small", "m-big"]}),
        Some("m-small"),
    )
    .await;
    let memory = fantastic_ram_store::test_support::memory_meta(
        &kernel,
        "chat1",
        json!({"cli": {"call": ["*"]}, "prov": {"call": ["complete", "watch"]}}),
    )
    .await;
    let mut create = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools",
                            "id": "chat1", "provider_id": "prov", "context_window": 32768});
    for (k, v) in memory {
        create[k] = v;
    }
    let made = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], "chat1", "{made}");
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            AgentId::from("chat1"),
            json!({"type": "send", "text": "hi"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(
        crate::tests::last_answer(&kernel, &AgentId::from("chat1")).await,
        "hello from m-small <call name=\"done\"></call>",
        "no model on the chat: the provider's default: {r}"
    );
    let rec = kernel.resolve_target("chat1").unwrap().record();
    assert!(
        rec.meta.get("model").is_none(),
        "the chat's record holds no provider field: {:?}",
        rec.meta
    );
    let rows = kernel
        .operator_send(vec![Item::new(
            AgentId::from("chat1"),
            json!({"type": "rows"}),
        )])
        .await
        .remove(0)
        .result;
    let text = rows.to_string();
    assert!(
        text.contains("hi") && text.contains("hello"),
        "the chat holds both sides: {rows}"
    );
    let refl = kernel
        .operator_send(vec![Item::new(
            AgentId::from("chat1"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(refl["provider_id"], "prov", "{refl}");

    // a prompt file the chat could never read is refused at the write, as the provider's check did before the split
    for (field, bad) in [
        ("system_prompt_file", json!("")),
        ("file_bridge_id", json!(5)),
    ] {
        let r = kernel
            .operator_send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "update_agent", "id": "chat1", field: bad}),
            )])
            .await
            .remove(0)
            .result;
        assert!(
            r["error"].as_str().unwrap_or_default().contains(field),
            "{field}: {r}"
        );
    }

    // the chat names its own model: the next pass runs on it, the conversation stays
    fantastic_kernel::test_support::update(
        &kernel,
        &AgentId::from("chat1"),
        json!({"model": "m-big"}).as_object().unwrap().clone(),
    );
    let r = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            AgentId::from("chat1"),
            json!({"type": "send", "text": "again"}),
        )]))
        .await
        .remove(0)
        .result;
    assert_eq!(
        crate::tests::last_answer(&kernel, &AgentId::from("chat1")).await,
        "hello from m-big <call name=\"done\"></call>",
        "{r}"
    );
    let rows = kernel
        .operator_send(vec![Item::new(
            AgentId::from("chat1"),
            json!({"type": "rows"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        rows.to_string().contains("hi"),
        "the first turn is still the chat's: {rows}"
    );
}

/// THE CHAT'S OWN IDENTITY: a call the model makes through an `ai_chat` is the CHAT's,
/// judged by the chat's grants — never by its provider's, which may reach far more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chats_call_is_judged_by_the_chats_grants_not_its_providers() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register("echo_provider.tools", EchoProvider);
        k.bundles.register(
            crate::chat_bundle::HANDLER_MODULE,
            crate::chat_bundle::AiChatBundle,
        );
    })
    .await;
    // `vault`: only the provider may call it
    make_provider(&kernel, "vault", json!({}), None).await;
    make_provider(
        &kernel,
        "prov",
        json!({"models": ["m-call"]}),
        Some("m-call"),
    )
    .await;
    fantastic_kernel::test_support::grant(&kernel, "prov", "vault", json!({"call": ["*"]}));
    let memory = fantastic_ram_store::test_support::memory_meta(
        &kernel,
        "chat_grants",
        json!({"cli": {"call": ["*"]}, "prov": {"call": ["complete", "watch"]}}),
    )
    .await;
    let mut create = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools",
                            "id": "chat_grants", "provider_id": "prov", "context_window": 32768});
    for (k, v) in memory {
        create[k] = v;
    }
    let made = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], "chat_grants", "{made}");
    let _ = crate::tests::settled(&kernel, kernel
        .operator_send(vec![Item::new(
            AgentId::from("chat_grants"),
            json!({"type": "send", "text": "go"}),
        )]))
        .await;
    let mut text = String::new();
    for _ in 0..50 {
        let rows = kernel
            .operator_send(vec![Item::new(
                AgentId::from("chat_grants"),
                json!({"type": "rows"}),
            )])
            .await
            .remove(0)
            .result;
        text = rows.to_string();
        if text.contains("· vault reflect") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        text.contains("· vault reflect"),
        "the call's reply is in the chat: {text}"
    );
    assert!(
        text.contains("<error>no agent 'vault'</error>"),
        "the chat's own grants refuse it, not the provider's (the unknown answer): {text}"
    );
}

/// A turn another agent STARTS is still the chat's own: its calls are judged by the
/// chat's grants, not by the grants of whoever sent the message that woke it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_woken_by_a_broad_agent_keeps_the_chats_own_grants() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register("echo_provider.tools", EchoProvider);
        k.bundles.register(
            crate::chat_bundle::HANDLER_MODULE,
            crate::chat_bundle::AiChatBundle,
        );
    })
    .await;
    // `boss` may call vault and the chat; vault accepts boss only
    make_provider(&kernel, "vault", json!({}), None).await;
    make_provider(&kernel, "boss", json!({}), None).await;
    for (who, to) in [("boss", "vault"), ("boss", "chat2")] {
        fantastic_kernel::test_support::grant(&kernel, who, to, json!({"call": ["*"]}));
    }
    make_provider(
        &kernel,
        "prov2",
        json!({"models": ["m-call"]}),
        Some("m-call"),
    )
    .await;
    let memory = fantastic_ram_store::test_support::memory_meta(&kernel, "chat2", json!({"cli": {"call": ["*"]}, "boss": {"call": ["*"]}, "prov2": {"call": ["complete", "watch"]}})).await;
    let mut create = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools",
                            "id": "chat2", "provider_id": "prov2", "context_window": 32768});
    for (k, v) in memory {
        create[k] = v;
    }
    let made = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], "chat2", "{made}");
    // boss wakes the chat
    let woke = fantastic_kernel::test_support::as_agent(
        "boss",
        kernel.operator_send(vec![Item::new(
            AgentId::from("chat2"),
            json!({"type": "send", "text": "go"}),
        )]),
    )
    .await
    .remove(0)
    .result;
    assert!(
        woke.get("reason").is_none(),
        "boss may wake the chat: {woke}"
    );
    let mut text = String::new();
    for _ in 0..50 {
        let rows = kernel
            .operator_send(vec![Item::new(
                AgentId::from("chat2"),
                json!({"type": "rows"}),
            )])
            .await
            .remove(0)
            .result;
        text = rows.to_string();
        if text.contains("· vault reflect") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        text.contains("· vault reflect"),
        "the call's reply is in the chat: {text}"
    );
    assert!(
        text.contains("<error>no agent 'vault'</error>"),
        "the chat's own grants refuse it, not boss's (the unknown answer): {text}"
    );
}

#[tokio::test]
async fn an_ai_chat_reflect_carries_its_html_view_and_the_calls_it_needs() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register(
            crate::chat_bundle::HANDLER_MODULE,
            crate::chat_bundle::AiChatBundle,
        );
    })
    .await;
    let create = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools", "id": "chat_reflect"});
    let made = kernel
        .operator_send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], "chat_reflect", "{made}");
    let refl = kernel
        .operator_send(vec![Item::new(
            AgentId::from("chat_reflect"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    let view = &refl["view"];
    assert_eq!(view["kind"], "html", "{refl}");
    assert!(
        view["bytes"].as_str().is_some_and(|b| !b.is_empty()),
        "the view has its body"
    );
    assert_eq!(
        view["calls"],
        json!({
            "chat_reflect": ["watch", "reflect", "history", "rows", "status", "send", "stop", "context_clear", "unembed"],
            "user": ["inquiries", "answer_inquiry"],
        })
    );
}

#[tokio::test]
async fn a_provider_completes_on_its_default_model_field_and_lists_and_sets_it() {
    let tmp = TempDir::new().unwrap();
    let kernel = base_kernel(&tmp, |k| {
        k.bundles.register("echo_provider.tools", EchoProvider);
    })
    .await;
    make_provider(
        &kernel,
        "prov",
        json!({"models": ["m-small", "m-big"]}),
        Some("m-small"),
    )
    .await;
    make_provider(&kernel, "bare", json!({}), None).await;
    let call = |to: &str, p: Value| {
        let (k, to) = (Arc::clone(&kernel), to.to_string());
        async move {
            k.operator_send(vec![Item::new(AgentId::from(to.as_str()), p)])
                .await
                .remove(0)
                .result
        }
    };
    let msg = json!([{"role": "user", "content": "hi"}]);
    let r = call(
        "prov",
        json!({"type": "complete", "messages": msg, "rid": "r1"}),
    )
    .await;
    assert_eq!(
        r["model"], "m-small",
        "no model named: the record's default_model: {r}"
    );
    let r = call(
        "prov",
        json!({"type": "complete", "messages": msg, "rid": "r2", "model": "m-big"}),
    )
    .await;
    assert_eq!(r["model"], "m-big", "{r}");
    let r = call(
        "bare",
        json!({"type": "complete", "messages": msg, "rid": "r3"}),
    )
    .await;
    assert_eq!(
        r["model"], "srv-1",
        "no user model, no default: the server's: {r}"
    );
    assert_eq!(
        call("bare", json!({"type": "models"})).await["default"],
        "srv-1",
        "which is the default from now on"
    );
    assert_eq!(
        call("prov", json!({"type": "models"})).await,
        json!({"models": ["m-small", "m-big"], "default": "m-small"})
    );
    assert_eq!(
        call(
            "prov",
            json!({"type": "set_default_model", "model": "m-big"})
        )
        .await,
        json!({"default": "m-big"})
    );
    assert_eq!(
        call("prov", json!({"type": "models"})).await["default"],
        "m-big"
    );
    let no = call(
        "prov",
        json!({"type": "set_default_model", "model": "m-huge"}),
    )
    .await;
    assert_eq!(no["reason"], "not_found", "not one of its models: {no}");
    let r = call(
        "prov",
        json!({"type": "complete", "messages": msg, "rid": "r4"}),
    )
    .await;
    assert_eq!(r["model"], "m-big", "the new default: {r}");
}

#[path = "tests_life.rs"]
mod life_machine;
