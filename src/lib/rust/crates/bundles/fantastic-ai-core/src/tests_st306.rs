//! #306: an `await` waits for the agent's own running exec job, and the job's end
//! wakes its owner once with `job_done`. Self-contained like `tests_pause.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::agent_loop::BackendConfig;
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
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
    provider_cache: false,
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

/// Fake exec jobs that may finish, keyed by exec agent id. A job runs until released.
static RELEASED: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

fn released() -> &'static Mutex<HashMap<String, bool>> {
    RELEASED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn release(exec: &str) {
    released().lock().unwrap().insert(exec.to_string(), true);
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
                r#"<call name="await"></call>"#.to_string()
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
/// awaits; any other agent just awaits.
fn first_pass(agent: &str) -> String {
    match agent.strip_suffix("_job") {
        Some(base) => {
            let mut args = Map::new();
            args.insert("target_id".into(), json!(format!("{base}_exec")));
            args.insert("payload".into(), json!({"type": "start", "command": "job"}));
            format!(
                "{}<call name=\"await\"></call>",
                crate::tool_parse::render_call("send", &args)
            )
        }
        None => r#"<call name="await"></call>"#.to_string(),
    }
}

struct MockBundle;

#[async_trait]
impl Bundle for MockBundle {
    fn name(&self) -> &str {
        CFG.name
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let served = served_for(agent_id.as_str());
        let first = first_pass(agent_id.as_str());
        let reply = crate::verbs::handle_common(agent_id, payload, kernel, CFG, move |_client| {
            Arc::new(Mock {
                served: Arc::clone(&served),
                first: first.clone(),
            }) as Arc<dyn Provider>
        })
        .await?
        .unwrap_or_else(|| json!({"error": "st306_mock: unknown type"}));
        Ok(Some(reply))
    }
}

/// The fake exec agent. `start` runs job 1; `status` reports it done once released
/// (and otherwise sleeps briefly, as a live wait would); `tail` returns its output.
struct FakeExec;

#[async_trait]
impl Bundle for FakeExec {
    fn name(&self) -> &str {
        "st306_fake_exec"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let id = agent_id.as_str().to_string();
        let reply = match payload.get("type").and_then(Value::as_str) {
            Some("start") => json!({"job_id": 1, "status": "running", "pty": false}),
            Some("status") => {
                let done = released()
                    .lock()
                    .unwrap()
                    .get(&id)
                    .copied()
                    .unwrap_or(false);
                if done {
                    json!({"jobs": [{"job_id": 1, "status": "done", "exit_code": 0}]})
                } else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    json!({"jobs": [{"job_id": 1, "status": "running"}]})
                }
            }
            Some("tail") => json!({
                "job_id": 1,
                "status": "done",
                "lines": [{"n": 1, "stream": "stdout", "line": "hello from the job"}]
            }),
            _ => json!({"error": "st306_fake_exec: unknown type"}),
        };
        Ok(Some(reply))
    }
}

async fn create_one(kernel: &Arc<Kernel>, create: Value) {
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
}

/// A kernel with the mock backend, the fake exec agent and the given agents on it.
/// Each agent may call the exec agent; the exec agent takes calls from each agent.
async fn setup(tmp: &TempDir, agents: &[&str], exec: &str) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
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
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    fantastic_ram_store::test_support::ensure_memory(&kernel).await;
    for id in agents {
        let memory =
            fantastic_ram_store::test_support::memory_meta(&kernel, id, json!({"cli": "*"})).await;
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
    let callers: Map<String, Value> = agents.iter().map(|a| (a.to_string(), json!("*"))).collect();
    create_one(
        &kernel,
        json!({
            "type": "create_agent",
            "parent_id": "kernel",
            "handler_module": EXEC,
            "id": exec,
            "callable_by": Value::Object(callers),
        }),
    )
    .await;
    // Each agent's `can_call` names the exec agent (update REPLACES the field, so the
    // baseline grants ride along).
    for id in agents {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type": "update_agent",
                    "id": id,
                    "can_call": {
                        "$self": "*", "$parent": "*", "$subtree": "*",
                        "cli": "*", fantastic_ram_store::test_support::MEMORY: "*", exec: "*",
                    },
                }),
            )])
            .await
            .remove(0)
            .result;
    }
    kernel
}

async fn send(kernel: &Arc<Kernel>, to: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(to), payload)])
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

/// The wake a job's end delivers. Its text is `job_done {json}`.
const WAKE: &str = "job_done {";

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

fn owns_job(owner: &str) -> bool {
    crate::route::running_jobs()
        .lock()
        .unwrap()
        .get(owner)
        .is_some_and(|v| !v.is_empty())
}

/// A status event of `phase` whose detail carries `reason`.
fn status_is(e: &Value, phase: &str, reason: &str) -> bool {
    e.get("type").and_then(Value::as_str) == Some("status")
        && e.get("phase").and_then(Value::as_str) == Some(phase)
        && e.pointer("/detail/reason").and_then(Value::as_str) == Some(reason)
}

/// An `awaiting` status that counted at least one pending piece of work.
fn awaiting_with_pending(seen: &[Value]) -> bool {
    seen.iter().any(|e| {
        status_is(e, "done", "awaiting")
            && e.pointer("/detail/pending")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= 1
    })
}

fn nothing_to_await(seen: &[Value]) -> bool {
    seen.iter()
        .any(|e| status_is(e, "done", "nothing_to_await"))
}

#[tokio::test]
async fn await_with_a_running_job_waits_for_it() {
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
        owns_job(owner) && awaiting_with_pending(seen)
    })
    .await;
    assert!(
        waiting,
        "the await did not count the running job: recorded={} events={seen:?}",
        owns_job(owner)
    );
    assert!(
        !nothing_to_await(&seen),
        "the await ended as nothing to wait for"
    );
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
    assert!(
        woken.contains("hello from the job"),
        "no tail in the wake: {woken}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wakes(&served), 1, "the owner was woken more than once");
    assert!(!owns_job(owner), "a finished job is still recorded");
}

#[tokio::test]
async fn await_with_no_job_and_nothing_pending_still_ends_visibly() {
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
    let ended = poll_events(&mut rx, &mut seen, nothing_to_await).await;
    assert!(
        ended,
        "a bare await with nothing pending did not end visibly: {seen:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        served.requests.lock().unwrap().len(),
        1,
        "an await with nothing pending woke the agent again"
    );
    assert!(!owns_job(idle), "an idle agent has a job recorded");
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
    let ended = poll_events(&mut rx, &mut seen, nothing_to_await).await;
    assert!(ended, "the bystander's await did not end visibly: {seen:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        wakes(&served_for(other)),
        0,
        "a bystander was woken by another agent's job"
    );
    assert!(!owns_job(other), "a job was recorded for the bystander");
}

#[tokio::test]
async fn a_deleted_owner_stops_counting_its_job() {
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
        owns_job(owner) && awaiting_with_pending(seen)
    })
    .await;
    assert!(waiting, "the job was never recorded: {seen:?}");
    let _ = send(
        &kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": owner}),
    )
    .await;
    release(exec);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !owns_job(owner),
        "a deleted owner still counts its job as pending"
    );
}
