//! #306: an agent that starts an exec job ends its answer with `done` (#795), and the
//! job's end wakes its owner once: the `start` call settles when its job exits, and that
//! settle is delivered to the starter as its reply. Self-contained like `tests_pause.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::agent_loop::BackendConfig;
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use crate::tests_decl::chat_decls;
use fantastic_kernel::inbox::InboxReceiver;
use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

const HANDLER: &str = "st306_mock.tools";
const EXEC: &str = "st306_fake_exec.tools";
/// A non-default client id, so the agent's status events land on its own inbox.
const CLIENT: &str = "st306_client";

const CFG: BackendConfig = BackendConfig {
    name: "st306_mock",
    model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
};

/// Every model request one agent was served, in order.
#[derive(Default)]
struct Served {
    requests: Mutex<Vec<Vec<Value>>>,
}

static SERVED: OnceLock<Mutex<HashMap<String, Arc<Served>>>> = OnceLock::new();

fn served_for(agent: &str) -> Arc<Served> {
    let map = SERVED.get_or_init(|| Mutex::new(HashMap::new()));
    Arc::clone(map.lock().unwrap().entry(agent.to_string()).or_default())
}

/// A fake exec job: running until released, then ended. A `start` that found it
/// running deferred its settle here, and the release settles it (as the exec bridge's
/// pump settles a `start` or a `run` at its job's exit).
#[derive(Default)]
struct FakeJob {
    released: bool,
    waiting: Option<(fantastic_kernel::TaskId, Arc<Kernel>)>,
}

static JOBS: OnceLock<Mutex<HashMap<String, FakeJob>>> = OnceLock::new();

fn jobs() -> &'static Mutex<HashMap<String, FakeJob>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The job's end, as its `start` is answered: its exit code and its last lines.
fn ended() -> Value {
    json!({
        "job_id": 1,
        "status": "done",
        "exit_code": 0,
        "lines": [{"n": 39, "stream": "stdout", "line": "hello from the job"}],
        "printed": 39
    })
}

/// The job of `exec` ends: a `start` waiting for it settles now.
fn release(exec: &str) {
    let waiting = {
        let mut g = jobs().lock().unwrap();
        let j = g.entry(exec.to_string()).or_default();
        j.released = true;
        j.waiting.take()
    };
    if let Some((pid, kernel)) = waiting {
        kernel.settle_deferred(
            pid,
            fantastic_kernel::Outcome::new(Some(pid), fantastic_kernel::Status::Completed, ended()),
        );
    }
}

struct Mock {
    served: Arc<Served>,
    first: String,
}

#[async_trait]
impl Provider for Mock {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
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
        "st306-model".to_string()
    }
}

/// The first pass of an agent. A `_job` agent starts a job on its `_exec` agent and
/// is done for now; any other agent is just done.
fn first_pass(agent: &str) -> String {
    match agent.strip_suffix("_job") {
        Some(base) => {
            let mut args = Map::new();
            args.insert("target_id".into(), json!(format!("{base}_exec")));
            args.insert("payload".into(), json!({"type": "start", "command": "job"}));
            format!(
                "{}<call name=\"done\"></call>",
                crate::tool_parse::render_call("send", &args)
            )
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
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let served = served_for(agent_id.as_str());
        let first = first_pass(agent_id.as_str());
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
            || json!({"id": "st306_mock"}),
        )
        .await?;
        Ok(Some(reply))
    }
}

/// The fake exec agent. `start` runs job 1 and settles when it ends (at once when it
/// was released already); `status` and `tail` are snapshots.
struct FakeExec;

/// `start` of the fake exec agent: a command to run.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct StartJob {
    /// What to run.
    command: String,
}

/// `status` of the fake exec agent: the jobs it runs.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct JobStatus {
    /// One job; absent, all.
    job_id: Option<i64>,
}

/// `tail` of the fake exec agent: a job's output.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct TailJob {
    /// The job.
    job_id: Option<i64>,
}

mod exec_verbs {
    use super::{JobStatus, StartJob, TailJob};
    fantastic_kernel::verbs! { "start" => StartJob, "status" => JobStatus, "tail" => TailJob }
}

#[async_trait]
impl Handler for FakeExec {
    type Call = exec_verbs::Call;
    fn name(&self) -> &str {
        "st306_fake_exec"
    }
    fn man(&self) -> &'static str {
        "A fake exec agent."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        exec_verbs::VERBS
    }
    fn dynamic_verbs(&self, a: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        exec_verbs::dynamic_verbs(a)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &crate::tests_decl::any::RECORD
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        use exec_verbs::Call;
        let id = agent_id.as_str().to_string();
        let done = jobs().lock().unwrap().get(&id).is_some_and(|j| j.released);
        let reply = match call {
            Call::Reflect(_) => json!({"id": id}),
            Call::StartJob(_) if done => ended(),
            Call::StartJob(_) => {
                let d = kernel.defer().map_err(|e| format!("defer: {e}"))?;
                jobs().lock().unwrap().entry(id).or_default().waiting =
                    Some((d.pid(), Arc::clone(kernel)));
                Value::Null
            }
            Call::JobStatus(_) if done => {
                json!({"jobs": [{"job_id": 1, "status": "done", "exit_code": 0}]})
            }
            Call::JobStatus(_) => json!({"jobs": [{"job_id": 1, "status": "running"}]}),
            Call::TailJob(_) => ended(),
        };
        Ok(Some(reply))
    }
}

async fn create_one(kernel: &Arc<Kernel>, create: Value) {
    let _ = fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
}

/// A kernel with the mock backend, the fake exec agent and the given agents on it.
/// Each agent may call the exec agent; the exec agent takes calls from each agent.
async fn setup(tmp: &TempDir, agents: &[&str], exec: &str) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
    kernel.bundles.register(HANDLER, MockBundle);
    kernel.bundles.register(EXEC, FakeExec);
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    fantastic_ram_store::test_support::ensure_memory(&kernel).await;
    for id in agents {
        let memory =
            fantastic_ram_store::test_support::memory_meta(&kernel, id, json!({"cli": {"call": ["*"]}})).await;
        let mut create = json!({
            "type": "create_agent",
            "parent_id": "kernel",
            "handler_module": HANDLER,
            "id": id,
            "model": "st306-model",
            "context_window": 32768,
        });
        for (k, v) in memory {
            create[k] = v;
        }
        create_one(&kernel, create).await;
    }
    create_one(
        &kernel,
        json!({
            "type": "create_agent",
            "parent_id": "kernel",
            "handler_module": EXEC,
            "id": exec,
        }),
    )
    .await;
    // Each agent holds its own line on the exec agent (update REPLACES the field, so
    // every line rides along).
    for id in agents {
        let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type": "update_agent",
                    "id": id,
                    "can": {"$parent": {"call": ["*"]}, "cli": {"call": ["*"]}, fantastic_ram_store::test_support::MEMORY: {"call": ["*"]}, exec: {"call": ["*"]}},
                }),
            )])
            .await
            .remove(0)
            .result;
    }
    kernel
}

async fn send(kernel: &Arc<Kernel>, to: &str, payload: Value) -> Value {
    fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(AgentId::from(to), payload)])
        .await
        .remove(0)
        .result
}

/// Pull everything queued on `rx` into `seen`, until `ok` holds (about 5 seconds at most).
async fn poll_events(
    rx: &mut InboxReceiver,
    seen: &mut Vec<Value>,
    ok: impl Fn(&[Value]) -> bool,
) -> bool {
    for _ in 0..250 {
        while let Some(v) = rx.try_recv() {
            seen.push(v.payload);
        }
        if ok(seen) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// The first served request containing `needle`, rendered (about 5 seconds).
async fn await_request(served: &Arc<Served>, needle: &str) -> Option<String> {
    for _ in 0..250 {
        if let Some(found) = last_request_with(served, needle) {
            return Some(found);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// The last served request containing `needle`, rendered.
fn last_request_with(served: &Arc<Served>, needle: &str) -> Option<String> {
    served
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .map(|r| Value::Array(r.clone()).to_string())
        .find(|s| s.contains(needle))
}

/// The wake a job's end delivers: the reply of the `start` that started it.
const WAKE: &str = "_exec start";

fn wakes(served: &Arc<Served>) -> usize {
    served
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| Value::Array(r.clone()).to_string().matches(WAKE).count())
        .max()
        .unwrap_or(0)
}

/// The agent owes a call whose reply has not come: its job's `start` is open.
fn owes(owner: &str) -> bool {
    let state = crate::state::state_for(&AgentId::from(owner));
    let open = state.life.lock().unwrap().open;
    open > 0
}

/// A status event of `phase` whose detail carries `reason`.
fn status_is(e: &Value, phase: &str, reason: &str) -> bool {
    e.get("type").and_then(Value::as_str) == Some("status")
        && e.get("phase").and_then(Value::as_str) == Some(phase)
        && e.pointer("/detail/reason").and_then(Value::as_str) == Some(reason)
}

/// The pass ended visibly: a `done` status with `reason: ok`.
fn ended_done(seen: &[Value]) -> bool {
    seen.iter().any(|e| status_is(e, "done", "ok"))
}

/// `done` with a job running: the pass ends, the `start` stays open (owed), and nothing
/// wakes the owner before the job ends.
#[tokio::test]
async fn done_with_a_running_job_ends_and_its_start_stays_open() {
    let tmp = TempDir::new().unwrap();
    let (owner, exec) = ("st306_w1_job", "st306_w1_exec");
    let kernel = setup(&tmp, &[owner], exec).await;
    let served = served_for(owner);
    let mut rx = kernel.ensure_inbox(&AgentId::from(owner));
    let mut seen = Vec::new();
    let _ = send(
        &kernel,
        owner,
        json!({"type": "send", "text": "run the job", "client_id": CLIENT}),
    )
    .await;
    let waiting = poll_events(&mut rx, &mut seen, |seen| {
        owes(owner) && ended_done(seen)
    })
    .await;
    assert!(
        waiting,
        "the pass did not end with the start open: owed={} events={seen:?}",
        owes(owner)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        last_request_with(&served, WAKE).is_none(),
        "the owner was woken before its job ended"
    );
    release(exec);
}

#[tokio::test]
async fn a_finished_job_wakes_its_owner_once() {
    let tmp = TempDir::new().unwrap();
    let (owner, exec) = ("st306_w2_job", "st306_w2_exec");
    let kernel = setup(&tmp, &[owner], exec).await;
    release(exec);
    let served = served_for(owner);
    let _ = send(
        &kernel,
        owner,
        json!({"type": "send", "text": "run the job", "client_id": CLIENT}),
    )
    .await;
    let woken = await_request(&served, WAKE)
        .await
        .expect("the owner was never woken");
    assert!(
        woken.contains("exit_code"),
        "no exit code in the wake: {woken}"
    );
    assert!(woken.contains("[reply to "), "the job's end is not the start's reply: {woken}");
    assert!(
        woken.contains("hello from the job"),
        "no tail in the wake: {woken}"
    );
    // the job printed 39 lines, of which the tail shows the last
    assert!(woken.contains("printed") && woken.contains("39"), "no line total in the wake: {woken}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wakes(&served), 1, "the owner was woken more than once");
    assert!(!owes(owner), "a finished job's start is still owed");
}

/// The job ends AFTER the owner's pass ended with `done`: its `start` settles then, and
/// that settle wakes the idle owner once (#795: no watcher, no poll).
#[tokio::test]
async fn a_job_that_ends_later_wakes_its_idle_owner_once() {
    let tmp = TempDir::new().unwrap();
    let (owner, exec) = ("st306_w6_job", "st306_w6_exec");
    let kernel = setup(&tmp, &[owner], exec).await;
    let served = served_for(owner);
    let mut rx = kernel.ensure_inbox(&AgentId::from(owner));
    let mut seen = Vec::new();
    let _ = send(
        &kernel,
        owner,
        json!({"type": "send", "text": "run the job", "client_id": CLIENT}),
    )
    .await;
    assert!(
        poll_events(&mut rx, &mut seen, |seen| owes(owner) && ended_done(seen)).await,
        "the pass did not end with the start open: {seen:?}"
    );
    assert_eq!(served.requests.lock().unwrap().len(), 1);
    release(exec);
    let woken = await_request(&served, WAKE).await.expect("the job's end never woke its owner");
    assert!(woken.contains("hello from the job"), "no tail in the wake: {woken}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wakes(&served), 1, "the owner was woken more than once");
    assert_eq!(served.requests.lock().unwrap().len(), 2, "one pass per job end");
    assert!(!owes(owner), "a finished job's start is still owed");
}

/// `done` with nothing running ends visibly, and nothing wakes the agent again.
#[tokio::test]
async fn done_with_no_job_ends_visibly_and_nothing_wakes_it() {
    let tmp = TempDir::new().unwrap();
    let (idle, exec) = ("st306_w3_idle", "st306_w3_exec");
    let kernel = setup(&tmp, &[idle], exec).await;
    let served = served_for(idle);
    let mut rx = kernel.ensure_inbox(&AgentId::from(idle));
    let mut seen = Vec::new();
    let _ = send(
        &kernel,
        idle,
        json!({"type": "send", "text": "wait for nothing", "client_id": CLIENT}),
    )
    .await;
    let ended = poll_events(&mut rx, &mut seen, ended_done).await;
    assert!(ended, "a bare done did not end visibly: {seen:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        served.requests.lock().unwrap().len(),
        1,
        "a done with nothing running woke the agent again"
    );
    assert!(!owes(idle), "an idle agent owes a call");
}

#[tokio::test]
async fn a_job_started_by_another_agent_does_not_wake_this_one() {
    let tmp = TempDir::new().unwrap();
    let (owner, other, exec) = ("st306_w4_job", "st306_w4_idle", "st306_w4_exec");
    let kernel = setup(&tmp, &[owner, other], exec).await;
    release(exec);
    let owner_served = served_for(owner);
    let _ = send(
        &kernel,
        owner,
        json!({"type": "send", "text": "run the job", "client_id": CLIENT}),
    )
    .await;
    await_request(&owner_served, WAKE)
        .await
        .expect("the owner was never woken");
    let mut rx = kernel.ensure_inbox(&AgentId::from(other));
    let mut seen = Vec::new();
    let _ = send(
        &kernel,
        other,
        json!({"type": "send", "text": "wait for nothing", "client_id": CLIENT}),
    )
    .await;
    let ended = poll_events(&mut rx, &mut seen, ended_done).await;
    assert!(ended, "the bystander's done did not end visibly: {seen:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        wakes(&served_for(other)),
        0,
        "a bystander was woken by another agent's job"
    );
    assert!(!owes(other), "the bystander owes a call");
}

/// A deleted owner is not woken by its job's end: the `start` it made settles to no one.
#[tokio::test]
async fn a_deleted_owner_is_not_woken_by_its_jobs_end() {
    let tmp = TempDir::new().unwrap();
    let (owner, exec) = ("st306_w5_job", "st306_w5_exec");
    let kernel = setup(&tmp, &[owner], exec).await;
    let mut rx = kernel.ensure_inbox(&AgentId::from(owner));
    let mut seen = Vec::new();
    let _ = send(
        &kernel,
        owner,
        json!({"type": "send", "text": "run the job", "client_id": CLIENT}),
    )
    .await;
    let waiting = poll_events(&mut rx, &mut seen, |seen| {
        owes(owner) && ended_done(seen)
    })
    .await;
    assert!(waiting, "the start was never open: {seen:?}");
    let served = served_for(owner);
    let _ = send(
        &kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": owner}),
    )
    .await;
    release(exec);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wakes(&served), 0, "a deleted owner was woken by its job's end");
    assert_eq!(served.requests.lock().unwrap().len(), 1, "a deleted owner ran a pass");
}
