//! Recurring-task scheduler as an agent.
//!
//! State (store-relative sidecars `agents/{id}/…`, persisted THROUGH the loader
//! via `load_blob`/`persist_blob` — nothing to wire; a write failfasts only when
//! the loader has no store, reads are empty until then):
//!
//! - `schedules.yaml` — a YAML sequence of `{id, target, payload, interval_seconds, created_at, next_run, run_count, paused, timeout?}`
//! - `schedule_events.yaml` — a YAML STREAM, one `---` document per `schedule_fired`
//!   event, ring-trimmed to `HISTORY_MAX`.
//!
//! Both are written by `fantastic_kernel::yaml_io::dump` — the one emitter
//! config the kernel's own files use (block style, indent 2, key order kept).
//!
//! The verbs are declared in `verbs.rs`; `reflect` and `docs` render them.
//!
//! NO IDLE WAKEUPS: the fire loop sleeps until the EARLIEST
//! due schedule, or — none due, or the scheduler paused — until something
//! changes: a schedule added, removed, paused or resumed, or the scheduler's own
//! record updated (`paused`). An idle scheduler wakes never. Python `_tick_loop`.
//!
//! A SCHEDULE FIRES AS THE SCHEDULER. The tick loop is a task the scheduler's own
//! `boot` spawned, so every fire is the scheduler's send: the gate judges the
//! scheduler's `can_call` and the target's `callable_by` for the scheduler. Every caller the gate admits
//! sees, ticks, pauses, resumes and unschedules every schedule, and reads the
//! whole history.
//!
//! A tick collects every DUE schedule, bumps `run_count`/`next_run`, persists
//! FIRST (a crash inside the fire window cannot re-fire on reboot), then fires
//! them all in ONE `kernel.send([...])` batch — its items concurrent, so one hung
//! target never blocks the others. `timeout` is the schedule's own per-fire bound in
//! seconds; absent = wait forever (the kernel never defaults it). One
//! `{type:"schedule_fired", ...}` per outcome goes to the scheduler's OWN inbox
//! only — the target gets the payload, which is the tick.
//!
//! There is no `shutdown` VERB: the tick loop is a kernel task, stopped by deleting
//! the agent (`on_delete`) or by cancelling that task — the same two doors every
//! other bundle's background work has.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{py_repr_value, py_str};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::yaml_io;
use fantastic_kernel::{AgentId, Item, Kernel, Status, SubscriberToken};
use futures_util::FutureExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "scheduler.tools";

pub mod record;
pub mod verbs;

/// What the bundle is for — its one short man text; how to call it is rendered from
/// its declared verbs (`docs`, `reflect`).
const MAN: &str = "A recurring-task scheduler: it keeps a list of schedules, each a call to a target agent every so many seconds, fires the due ones as itself, and records every fire in its history.";

/// Maximum history entries kept per scheduler (ring-trim threshold).
/// Matches Python's `HISTORY_MAX`.
pub const HISTORY_MAX: usize = 500;

/// Per scheduler: what wakes its fire loop (a change), and the state tap that
/// pokes it when the scheduler's own record is updated. Python `_wake`/`_unsub`.
type Wakes = HashMap<AgentId, (Arc<Notify>, Option<SubscriberToken>)>;
static WAKES: OnceLock<Mutex<Wakes>> = OnceLock::new();

fn wakes() -> std::sync::MutexGuard<'static, Wakes> {
    WAKES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wake_of(agent_id: &AgentId) -> Arc<Notify> {
    Arc::clone(
        &wakes()
            .entry(agent_id.clone())
            .or_insert_with(|| (Arc::new(Notify::new()), None))
            .0,
    )
}

/// Something the fire loop waits on changed: it recomputes when to wake.
fn poke(agent_id: &AgentId) {
    if let Some((n, _)) = wakes().get(agent_id) {
        n.notify_one();
    }
}

/// Live tick tasks keyed by scheduler agent id. `boot` populates; `on_delete`
/// (and an operator's `cancel` on the row) aborts.
static SCHEDULER_TASKS: OnceLockTaskMap = OnceLockTaskMap::new();

/// In-process schedule cache, python's `_cache`. Every verb mutates it BEFORE it
/// persists, so the cache is what a failed write leaves behind, and nothing
/// ever evicts it — not even `delete_agent`.
static SCHEDULE_CACHE: OnceLockScheduleCache = OnceLockScheduleCache::new();

/// Per scheduler: one history append at a time, python's `_history_locks`. An
/// append reads the event file, adds one document and writes it back across
/// awaits; two fires (a schedule's own and a `tick_now`) interleaving there
/// would each write over the other's event.
static HISTORY_LOCKS: OnceLock<Mutex<HashMap<AgentId, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn history_locks() -> std::sync::MutexGuard<'static, HashMap<AgentId, Arc<tokio::sync::Mutex<()>>>>
{
    HISTORY_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ── once-lock map helpers ───────────────────────────────────────────

struct OnceLockTaskMap(OnceLock<Mutex<HashMap<AgentId, fantastic_kernel::TaskId>>>);
impl OnceLockTaskMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, fantastic_kernel::TaskId>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("SCHEDULER_TASKS poisoned")
    }
}

/// python configures NO logging handler, so `logger.warning` / `logger.exception`
/// reach `logging.lastResort` and print the BARE formatted message to stderr
/// (`scheduler/tools.py:43`). This is that writer, with a sink tests can swap in
/// to read the exact line back.
static LOG_SINK: OnceLockLogSink = OnceLockLogSink::new();

/// Emit one python `logging.lastResort` line.
fn log_line(msg: String) {
    if let Some(buf) = LOG_SINK.lock().as_mut() {
        buf.push(msg);
        return;
    }
    eprintln!("{msg}");
}

struct OnceLockLogSink(OnceLock<Mutex<Option<Vec<String>>>>);
impl OnceLockLogSink {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<String>>> {
        self.0
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("LOG_SINK poisoned")
    }
}

struct OnceLockScheduleCache(OnceLock<Mutex<HashMap<AgentId, Vec<Value>>>>);
impl OnceLockScheduleCache {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Vec<Value>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The scheduler bundle.
pub struct SchedulerBundle;

#[async_trait]
impl Handler for SchedulerBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        "scheduler"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        // `shutdown` is NOT a verb: the tick loop dies in `on_delete`, never behind
        // a verb.
        let reply = match call {
            verbs::Call::Reflect(_) => reflect_reply(agent_id, kernel),
            verbs::Call::Boot(_) => boot_reply(agent_id, kernel).await,
            verbs::Call::Schedule(a) => schedule_reply(agent_id, a, kernel).await,
            verbs::Call::Unschedule(a) => unschedule_reply(agent_id, &a.schedule_id, kernel).await,
            verbs::Call::List(_) => list_reply(agent_id, kernel).await,
            verbs::Call::Pause(a) => {
                set_paused(agent_id, a.schedule_id, kernel, "pause", "paused", true).await
            }
            verbs::Call::Resume(a) => {
                set_paused(agent_id, a.schedule_id, kernel, "resume", "resumed", false).await
            }
            verbs::Call::TickNow(a) => tick_now_reply(agent_id, &a.schedule_id, kernel).await,
            verbs::Call::History(a) => history_reply(agent_id, a, kernel).await,
        };
        Ok(Some(reply))
    }

    /// The tick loop and the SCHEDULE CACHE die with the agent.
    /// Python's `_cache` is a module global keyed by agent id; evicting it here
    /// keeps an agent recreated under the same id in the same process from
    /// seeing its predecessor's schedules.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        // The cache goes with the agent: a re-created scheduler with the same id
        // never lists or fires the old schedules.
        SCHEDULE_CACHE.lock().remove(agent_id);
        history_locks().remove(agent_id);
        shutdown_tick(agent_id, kernel).await;
        if let Some((_, Some(token))) = wakes().remove(agent_id) {
            kernel.remove_state_subscriber(token);
        }
        Ok(())
    }
    /// Shutdown frees what delete frees — declared, never a fallback.
    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.on_delete(agent_id, kernel).await
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn meta_value(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<Value> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).cloned()
}

/// A record flag: `true` only when the record says `true`.
fn meta_truthy(agent_id: &AgentId, kernel: &Kernel, key: &str) -> bool {
    meta_value(agent_id, kernel, key).and_then(|v| v.as_bool()) == Some(true)
}

/// python `kernel.update(id, **{key: value})`: the meta merges and the
/// `updated` state event names the changed key — which is what the root
/// loader's writer turns into the persist, as for every other update.
fn update_meta(agent_id: &AgentId, kernel: &Arc<Kernel>, key: &str, value: Value) {
    let mut patch = Map::new();
    patch.insert(key.to_string(), value);
    kernel.update(agent_id, patch);
}

// ── file-agent-routed persistence ───────────────────────────────────

// THE LOADER OWNS THE PATH. This bundle names only a FILENAME; the sidecar
// lands beside the agent's own agent.yaml in whatever store the kernel
// discovered. No provider is named here and no path is computed.
const SCHEDULES_BLOB: &str = "schedules.yaml";
const HISTORY_BLOB: &str = "schedule_events.yaml";

/// The root loader the sidecars persist through, or `None` when the root is not
/// a `kernel_state` (then a write fails loud and a read is empty). Python
/// `_loader`.
fn loader(kernel: &Kernel) -> Option<Arc<fantastic_kernel::Agent>> {
    kernel
        .root()
        .filter(|r| r.handler_module.as_deref() == Some("kernel_state.tools"))
}

/// ONE blob call to the root loader — persistence through the loader is KERNEL
/// BOOKKEEPING ([`Kernel::bookkeeping`]): the loader's handler runs directly, never
/// a gated send.
async fn loader_call(
    kernel: &Arc<Kernel>,
    root: &Arc<fantastic_kernel::Agent>,
    payload: Value,
) -> Value {
    kernel.bookkeeping(root, payload, None).await.result
}

/// Read a sidecar THROUGH the loader. Missing / no-store ⇒ `None` (reads lenient).
async fn file_read(agent_id: &AgentId, kernel: &Arc<Kernel>, name: &str) -> Option<String> {
    let root = loader(kernel)?;
    let reply = loader_call(
        kernel,
        &root,
        json!({"type": "load_blob", "agent_id": agent_id.0, "name": name}),
    )
    .await;
    reply
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A persist failure as the loader reported it: `{error, reason?}`.
type PersistError = Value;

/// Python's shape for a failed write inside a verb:
/// `{error: "scheduler.<verb>: <loader error>", reason?}`.
fn persist_err(verb: &str, e: &PersistError) -> Value {
    let msg = e.get("error").and_then(Value::as_str).unwrap_or("");
    let mut out = json!({"error": format!("scheduler.{verb}: {msg}")});
    if let Some(reason) = e.get("reason").and_then(Value::as_str) {
        out["reason"] = json!(reason);
    }
    out
}

async fn file_write(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    name: &str,
    content: &str,
) -> Result<(), PersistError> {
    let Some(root) = loader(kernel) else {
        return Err(
            json!({"error": "scheduler: the root is not a kernel_state loader — no store"}),
        );
    };
    let reply = loader_call(
        kernel,
        &root,
        json!({
            "type": "persist_blob",
            "agent_id": agent_id.0,
            "name": name,
            "content": content,
        }),
    )
    .await;
    if reply.get("error").and_then(Value::as_str).is_some() {
        let mut e = json!({"error": reply["error"]});
        if let Some(reason) = reply.get("reason") {
            e["reason"] = reason.clone();
        }
        return Err(e);
    }
    Ok(())
}

async fn load_schedules(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Vec<Value> {
    if let Some(cached) = SCHEDULE_CACHE.lock().get(agent_id).cloned() {
        return cached;
    }
    let raw = file_read(agent_id, kernel, SCHEDULES_BLOB).await;
    // python `_load`: a YAML sequence, anything else (absent, malformed, not a
    // list) reads as none.
    let parsed: Vec<Value> = match raw.map(|s| yaml_io::load(&s)) {
        Some(Ok(Value::Array(items))) => items,
        _ => Vec::new(),
    };
    // FIRST FILL WINS: a verb that loaded and wrote while this read was in flight
    // already holds the cache — the read must not replace it with the older list.
    SCHEDULE_CACHE
        .lock()
        .entry(agent_id.clone())
        .or_insert(parsed)
        .clone()
}

/// Write the schedules out — RAM CACHE FIRST.
///
/// python's `_load` hands back the cached LIST ITSELF and every verb mutates it in
/// place, so by the time `_save` runs the cache already holds the change: a failed
/// persist leaves it there (tick, unschedule, pause, resume keep the change), and
/// only `schedule`/`tick_now` — which roll back explicitly — do not. Warming the
/// cache on success alone made rust the opposite of python on every one of them.
///
/// The bytes are `yaml_io.dump(schedules)` — a block YAML sequence, the one
/// emitter config the kernel's own files use.
async fn save_schedules(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    schedules: &[Value],
) -> Result<(), PersistError> {
    SCHEDULE_CACHE
        .lock()
        .insert(agent_id.clone(), schedules.to_vec());
    let body = yaml_io::dump(&Value::Array(schedules.to_vec()));
    file_write(agent_id, kernel, SCHEDULES_BLOB, &body).await
}

async fn append_history(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    event: &Value,
) -> Result<(), PersistError> {
    let serial = Arc::clone(history_locks().entry(agent_id.clone()).or_default());
    let _one_at_a_time = serial.lock().await;
    let prev = file_read(agent_id, kernel, HISTORY_BLOB)
        .await
        .unwrap_or_default();
    // python `raw + yaml_io.row_to_doc(event)`: one `---` document per event.
    let mut combined = prev;
    combined.push_str(&event_doc(event));
    // Ring-trim if oversize: keep the newest HISTORY_MAX documents.
    let events = parse_events(&combined);
    let trimmed = if events.len() > 2 * HISTORY_MAX {
        events[events.len() - HISTORY_MAX..]
            .iter()
            .map(event_doc)
            .collect::<String>()
    } else {
        combined
    };
    file_write(agent_id, kernel, HISTORY_BLOB, &trimmed).await
}

async fn read_history(agent_id: &AgentId, kernel: &Arc<Kernel>, limit: usize) -> Vec<Value> {
    let raw = match file_read(agent_id, kernel, HISTORY_BLOB).await {
        Some(s) => s,
        None => return Vec::new(),
    };
    if raw.is_empty() {
        return Vec::new();
    }
    let events = parse_events(&raw);
    // python `_parse_events(raw)[-limit:]` — `[-0:]` is the whole list.
    let from = if limit == 0 {
        0
    } else {
        events.len().saturating_sub(limit)
    };
    events[from..].to_vec()
}

/// One event as one stream document (python `yaml_io.row_to_doc(event)`).
fn event_doc(event: &Value) -> String {
    format!("{}{}", yaml_io::SEPARATOR, yaml_io::dump(event))
}

/// The event stream's documents that are mappings. A malformed stream reads
/// EMPTY — reads are lenient, as python's `_parse_events` is.
fn parse_events(raw: &str) -> Vec<Value> {
    yaml_io::load_all(raw)
        .map(|docs| docs.into_iter().filter(Value::is_object).collect())
        .unwrap_or_default()
}

// ── tick loop ───────────────────────────────────────────────────────

async fn tick_loop(agent_id: AgentId, kernel: Arc<Kernel>) {
    let wake = wake_of(&agent_id);
    loop {
        if kernel.agents.get(&agent_id).is_none() {
            return; // agent was deleted; exit cleanly
        }
        let mut delay: Option<f64> = None;
        if !meta_truthy(&agent_id, &kernel, "paused") {
            let now = now_secs();
            let mut schedules = load_schedules(&agent_id, &kernel).await;
            let mut due: Vec<Value> = Vec::new();
            for sch in schedules.iter_mut() {
                if sch.get("paused").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let next_run = sch.get("next_run").and_then(Value::as_f64).unwrap_or(0.0);
                if now < next_run {
                    delay = Some(delay.map_or(next_run - now, |d: f64| d.min(next_run - now)));
                    continue;
                }
                // Persist BEFORE the fire: run_count/next_run are bumped and saved
                // first, so a crash inside the fire window does not re-fire the same
                // tick on reboot (at-most-once restart semantics).
                let count = sch.get("run_count").and_then(Value::as_u64).unwrap_or(0);
                let interval = sch
                    .get("interval_seconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(60.0);
                sch["run_count"] = json!(count + 1);
                sch["next_run"] = json!(now + interval);
                due.push(sch.clone());
            }
            if !due.is_empty() {
                if let Err(e) = save_schedules(&agent_id, &kernel, &schedules).await {
                    // python `logger.warning("scheduler %s: persist failed: %s", sid,
                    // err.get("error"))` — an ABSENT `error` key reads `None`.
                    log_line(format!(
                        "scheduler {agent_id}: persist failed: {}",
                        py_str(e.get("error").unwrap_or(&Value::Null))
                    ));
                }
                fire_many(&agent_id, &kernel, &due).await;
                continue;
            }
        }
        // Until the earliest due fire, or a change — never on a fixed tick.
        match delay {
            Some(d) => {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs_f64(d.max(0.0))) => {}
                    () = wake.notified() => {}
                }
            }
            None => wake.notified().await,
        }
    }
}

/// A schedule's `target`, python's way: whatever was stored, as text. `schedule`
/// only checks the field for TRUTHINESS, so a non-string target is stored verbatim
/// and fired verbatim.
fn sched_target(sch: &Value) -> String {
    match sch.get("target") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_str(other),
        None => String::new(),
    }
}

/// A schedule's `payload`: the stored call object, `{}` when none.
fn sched_payload(sch: &Value) -> Value {
    match sch.get("payload").filter(|v| v.is_object()) {
        Some(p) => p.clone(),
        None => json!({}),
    }
}

fn fire_item(sch: &Value) -> Item {
    let mut item = Item::new(
        AgentId::from(sched_target(sch).as_str()),
        sched_payload(sch),
    );
    // python `if sch.get("fire_timeout") is not None: item["timeout"] = sch["fire_timeout"]`
    // — the value AS STORED, so the kernel validates it and its prose prints the
    // number the schedule holds (`5` → `5s`, not `5.0s`).
    if let Some(t) = sch.get("fire_timeout").filter(|v| !v.is_null()) {
        item = item.with_raw_timeout(t.clone());
    }
    item
}

/// One fire's `(result, error)` pair for the `schedule_fired` event.
///
/// `completed` ⇒ result is the reply and error stays NULL — even when that reply
/// carries an `error` key, because the status is the verdict. Anything else ⇒
/// `f"{status}: {detail}"`, where `detail` is `res["error"]` for a dict (ABSENT
/// reads `None`) and the whole result otherwise, both through python's `str()`. A
/// cancelled fire therefore reads `cancelled: None`.
fn fire_outcome(status: Status, result: Value) -> (Value, Value) {
    if status == Status::Completed {
        return (result, Value::Null);
    }
    let detail = match &result {
        Value::Object(m) => m.get("error").cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    (Value::Null, json!(format!("{status}: {}", py_str(&detail))))
}

/// Fire every due schedule AS THE SCHEDULER — one batch send, its items
/// concurrent, no head-of-line blocking — then record one `schedule_fired` per
/// outcome, and return those events in `due` order.
async fn fire_many(agent_id: &AgentId, kernel: &Arc<Kernel>, due: &[Value]) -> Vec<Value> {
    let ts = now_secs();
    // AS THE SCHEDULER: this runs in its handler (`tick_now`) or in the tick
    // loop its own `boot` spawned — identity comes from that dispatch.
    let outcomes = kernel.send(due.iter().map(fire_item).collect()).await;
    let duration_ms = ((now_secs() - ts) * 1000.0) as i64;
    let mut events = Vec::with_capacity(due.len());
    for (sch, out) in due.iter().zip(outcomes) {
        let target = sched_target(sch);
        let payload = sched_payload(sch);
        let sched_id = sch.get("id").cloned().unwrap_or(Value::Null);
        let (result, error) = fire_outcome(out.status, out.result);
        let event = json!({
            "type": "schedule_fired",
            "schedule_id": sched_id,
            "scheduler_id": agent_id.as_str(),
            "target": target,
            "payload": payload,
            "result": result,
            "error": error,
            "ts": ts,
            "duration_ms": duration_ms,
        });
        if let Err(e) = append_history(agent_id, kernel, &event).await {
            // python `logger.warning("scheduler %s: history persist failed: %s",
            // sid, err.get("error"))`.
            log_line(format!(
                "scheduler {agent_id}: history persist failed: {}",
                py_str(e.get("error").unwrap_or(&Value::Null))
            ));
        }
        // ONE delivery per tick (py-canonical): the payload send IS the tick; a
        // target-echo of `schedule_fired` would double every delivery to a live
        // program's on_message. Telemetry rides the scheduler's own inbox and
        // schedule_events.yaml.
        kernel.emit(agent_id, event.clone()).await;
        events.push(event);
    }
    events
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// python `scheduler.tools`' `f"sch_{secrets.token_hex(3)}"` — 3 bytes from
/// the OS CSPRNG as 6 lowercase hex. A failure is FATAL — no second source.
fn mint_id() -> String {
    let mut buf = [0u8; 3];
    fantastic_kernel::lifecycle::fill_random(&mut buf);
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("sch_{hex}")
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    // LIVENESS COMES FROM THE TABLE, which holds only running work — there is no
    // second flag to drift from it.
    let running = SCHEDULER_TASKS
        .lock()
        .get(agent_id)
        .map(|t| kernel.tasks.contains(t))
        .unwrap_or(false);
    json!({
        "id": agent_id.as_str(),
        "sentence": "Recurring-task scheduler.",
        "paused": meta_truthy(agent_id, kernel, "paused"),
        "running": running,
    })
}

async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    // The scheduler's own record changed (`paused`, by a verb or an update_agent):
    // the loop recomputes when to wake.
    {
        let mut w = wakes();
        let slot = w
            .entry(agent_id.clone())
            .or_insert_with(|| (Arc::new(Notify::new()), None));
        if slot.1.is_none() {
            let me = agent_id.clone();
            let n = Arc::clone(&slot.0);
            slot.1 = Some(kernel.add_state_subscriber(Arc::new(move |ev: &Value, _| {
                if ev.get("agent_id").and_then(Value::as_str) == Some(me.as_str())
                    && ev.get("kind").and_then(Value::as_str) == Some("updated")
                {
                    n.notify_one();
                }
            })));
        }
    }
    let mut tasks = SCHEDULER_TASKS.lock();
    if let Some(existing) = tasks.get(agent_id) {
        // LIVENESS COMES FROM THE TABLE, which holds only running work.
        if kernel.tasks.contains(existing) {
            // py `return {"running": True}` on BOTH branches: boot is
            // idempotent and says nothing else, so a caller cannot branch on
            // whether it was first. A boot of a running loop makes it look again.
            drop(tasks);
            poke(agent_id);
            return json!({"running": true});
        }
    }
    let id = agent_id.clone();
    let k = Arc::clone(kernel);
    // REGISTERED: the tick loop outlives this `boot` dispatch and fires a send
    // per due schedule for the life of the agent. A bare spawn left the clock
    // invisible to `ps`, beyond `cancel`, and sending under no identity — so
    // every fire reached the gate as the OPERATOR.
    let task = kernel.spawn_task(agent_id, "tick_loop", async move {
        // python wraps the whole loop in `except Exception: logger.exception(
        // "scheduler tick crashed for %s", sid)` (`scheduler/tools.py:243-244`)
        // — a crash names the scheduler on stderr instead of vanishing. Rust's
        // equivalent of an escaped exception is a panic, so the loop runs inside
        // `catch_unwind`. The traceback body is not reproduced.
        let sid = id.clone();
        if AssertUnwindSafe(tick_loop(id, k))
            .catch_unwind()
            .await
            .is_err()
        {
            log_line(format!("scheduler tick crashed for {sid}"));
        }
    });
    tasks.insert(agent_id.clone(), task);
    json!({"running": true})
}

/// Stop this scheduler's tick loop. A LIFECYCLE step (`on_delete`), not a verb.
async fn shutdown_tick(agent_id: &AgentId, kernel: &Arc<Kernel>) {
    let removed = SCHEDULER_TASKS.lock().remove(agent_id);
    if let Some(tid) = removed {
        // Through the TABLE — the same path an operator's `cancel` takes.
        kernel.cancel_task(&tid).await;
    }
}

/// `schedule` — the declaration checked the shape; what a type cannot say stays here.
async fn schedule_reply(agent_id: &AgentId, a: verbs::Schedule, kernel: &Arc<Kernel>) -> Value {
    let sched_payload = Value::Object(a.payload);
    let interval = a.interval_seconds;
    let target = json!(a.target);
    // Per-fire bound, seconds. Optional: absent = wait forever (never defaulted).
    // It is STORED AS SENT — an int `5` stays `5`, it does not become `5.0`.
    let timeout = a.fire_timeout.map(Value::Number);
    let now = now_secs();
    let mut sch = json!({
        "id": mint_id(),
        "target": target,
        "payload": sched_payload,
        "interval_seconds": interval,
        "created_at": now,
        "next_run": now + interval as f64,
        "run_count": 0,
        "paused": false,
    });
    if let Some(t) = timeout {
        sch["fire_timeout"] = t;
    }
    let mut schedules = load_schedules(agent_id, kernel).await;
    schedules.push(sch.clone());
    if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
        // ROLL THE CACHE BACK, as python does: `save_schedules` has already put the
        // new schedule there, and a failed persist must leave no phantom.
        let mut cache = SCHEDULE_CACHE.lock();
        let kept: Vec<Value> = cache
            .get(agent_id)
            .map(|v| {
                v.iter()
                    .filter(|s| s.get("id") != sch.get("id"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        cache.insert(agent_id.clone(), kept);
        drop(cache);
        return persist_err("schedule", &e);
    }
    poke(agent_id);
    json!({"schedule_id": sch["id"], "schedule": sch})
}

/// Does this stored schedule carry `sid`? python compares with `==` against the RAW
/// value, so only a string can ever match a stored string id.
fn is_sched(s: &Value, sid: &Value) -> bool {
    s.get("id") == Some(sid)
}

async fn unschedule_reply(agent_id: &AgentId, sid: &str, kernel: &Arc<Kernel>) -> Value {
    let sid = json!(sid);
    let mut schedules = load_schedules(agent_id, kernel).await;
    let before = schedules.len();
    schedules.retain(|s| !is_sched(s, &sid));
    let removed = schedules.len() < before;
    if removed {
        if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
            return persist_err("unschedule", &e);
        }
        poke(agent_id);
    }
    json!({"removed": removed, "schedule_id": sid})
}

async fn list_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let schedules: Vec<Value> = load_schedules(agent_id, kernel).await;
    json!({
        "schedules": schedules,
        "origin": format!("{agent_id} list — re-send; changes as schedules change"),
    })
}

/// `pause`/`resume` — one schedule, or the whole scheduler.
///
/// The per-schedule reply leads with the FLAG, then the id (python's key order).
async fn set_paused(
    agent_id: &AgentId,
    schedule_id: Option<String>,
    kernel: &Arc<Kernel>,
    verb: &str,
    flag_key: &str,
    paused: bool,
) -> Value {
    if let Some(sid) = schedule_id.map(Value::from) {
        let mut schedules = load_schedules(agent_id, kernel).await;
        let mut touched = 0;
        for s in schedules.iter_mut() {
            if is_sched(s, &sid) {
                s["paused"] = json!(paused);
                touched += 1;
            }
        }
        if touched > 0 {
            if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
                return persist_err(verb, &e);
            }
            poke(agent_id);
        }
        return json!({flag_key: touched > 0, "schedule_id": sid});
    }
    update_meta(agent_id, kernel, "paused", json!(paused));
    json!({flag_key: true, "scheduler_id": agent_id.as_str()})
}

async fn tick_now_reply(agent_id: &AgentId, sid: &str, kernel: &Arc<Kernel>) -> Value {
    let sid = json!(sid);
    let mut schedules = load_schedules(agent_id, kernel).await;
    let Some(idx) = schedules.iter().position(|s| is_sched(s, &sid)) else {
        return json!({"error": format!("schedule {} not found", py_repr_value(&sid))});
    };
    // Same order as the tick: bump → persist → fire.
    let count = schedules[idx]
        .get("run_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    schedules[idx]["run_count"] = json!(count + 1);
    if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
        // ROLL THE BUMP BACK (py `s["run_count"] -= 1`): `save_schedules` already
        // wrote the incremented list into the cache.
        schedules[idx]["run_count"] = json!(count);
        SCHEDULE_CACHE.lock().insert(agent_id.clone(), schedules);
        return persist_err("tick_now", &e);
    }
    let due = vec![schedules[idx].clone()];
    // THE OUTCOME, not just the fact. `{fired:true}` alone hides a target that
    // refuses every fire (e.g. `{type:'tick'}` sent to an AI agent, which has no
    // such verb), and the model never learns the payload is wrong. Python
    // canonical.
    let ev = fire_many(agent_id, kernel, &due)
        .await
        .pop()
        .unwrap_or(Value::Null);
    json!({
        "fired": true,
        "schedule_id": sid,
        "result": ev.get("result").cloned().unwrap_or(Value::Null),
        "error": ev.get("error").cloned().unwrap_or(Value::Null),
    })
}

async fn history_reply(agent_id: &AgentId, a: verbs::History, kernel: &Arc<Kernel>) -> Value {
    let limit = a.limit.min(500) as usize;
    // every fire, then the newest `limit` of them.
    let fires: Vec<Value> = read_history(agent_id, kernel, 2 * HISTORY_MAX).await;
    let from = fires.len().saturating_sub(limit);
    let mut entries = fires[from..].to_vec();
    if let Some(sid) = a.schedule_id.map(Value::from) {
        entries.retain(|e| e.get("schedule_id") == Some(&sid));
    }
    let n = entries.len();
    json!({
        "history": entries,
        "count": n,
        "origin": format!("{agent_id} history — re-send; grows as schedules fire"),
    })
}

#[cfg(test)]
mod tests;
