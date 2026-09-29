//! Recurring-task scheduler as an agent.
//!
//! State (store-relative sidecars `agents/{id}/…`, persisted THROUGH the loader
//! via `load_blob`/`persist_blob` — nothing to wire; a write failfasts only when
//! the loader has no store, reads are empty until then):
//!
//! - `schedules.yaml` — a YAML sequence of `{id, target, payload, interval_seconds, next_run, paused, run_count, created_at, timeout?}`
//! - `schedule_events.yaml` — a YAML STREAM, one `---` document per `schedule_fired`
//!   event, ring-trimmed to `HISTORY_MAX`.
//!
//! Both are written by `fantastic_kernel::yaml_io::dump` — the one emitter
//! config the kernel's own files use (block style, indent 2, key order kept).
//!
//! Verbs (mirror Python verb-for-verb):
//!
//! - `reflect` → `{id, sentence, tick_sec, paused, running, verbs, emits}`
//! - `boot`    → idempotent. Starts the tick task; answers `{running:true}`.
//! - `schedule`     args `{target, payload, interval_seconds, timeout?}` → mint + persist
//! - `unschedule`   args `{schedule_id}`                       → remove + persist
//! - `list`                                                    → `{schedules}`
//! - `pause` / `resume` args `{schedule_id?}`                  → flip one or all
//! - `tick_now`     args `{schedule_id}`                       → fire immediately
//! - `history`      args `{limit?, schedule_id?}`              → tail of schedule_events.yaml
//!
//! A tick collects every DUE schedule, bumps `run_count`/`next_run`, persists
//! FIRST (a crash inside the fire window cannot re-fire on reboot), then fires
//! them all as ONE `kernel.send([...])` batch — concurrent, so one hung target
//! never blocks the others. `timeout` is the schedule's own per-fire bound in
//! seconds; absent = wait forever (the kernel never defaults it). One
//! `{type:"schedule_fired", ...}` per outcome goes to the scheduler's OWN inbox
//! only — the target gets the payload, which is the tick.
//!
//! There is no `shutdown` VERB: the tick loop is a kernel task, stopped by deleting
//! the agent (`on_delete`) or by cancelling that task — the same two doors every
//! other bundle's background work has.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::{py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::yaml_io;
use fantastic_kernel::{bad, AgentId, Item, Kernel, Status};
use futures_util::FutureExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "scheduler.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Maximum history entries kept per scheduler (ring-trim threshold).
/// Matches Python's `HISTORY_MAX`.
pub const HISTORY_MAX: usize = 500;

/// Seconds between tick-loop sweeps when the record does not name one
/// (python `DEFAULT_TICK_SEC`).
pub const DEFAULT_TICK_SEC: f64 = 1.0;

/// Live tick tasks keyed by scheduler agent id. `boot` populates; `on_delete`
/// (and an operator's `cancel` on the row) aborts.
static SCHEDULER_TASKS: OnceLockTaskMap = OnceLockTaskMap::new();

/// In-process schedule cache, python's `_cache`. Every verb mutates it BEFORE it
/// persists, so the cache is what a failed write leaves behind (A5.9), and nothing
/// ever evicts it — not even `delete_agent` (A5.14).
static SCHEDULE_CACHE: OnceLockScheduleCache = OnceLockScheduleCache::new();

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
/// to read the exact line back (A15.2).
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
impl Bundle for SchedulerBundle {
    fn name(&self) -> &str {
        "scheduler"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// Refuse a `tick_sec` that is not a positive number at WRITE time — the
    /// point of the mistake. Silently defaulting it (as the tick loop's
    /// `unwrap_or(1.0)` did) turns an operator's typo into a behaviour change
    /// nobody is told about.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
    ) -> Option<String> {
        let raw = meta.get("tick_sec").or_else(|| record.get("tick_sec"))?;
        match raw.as_f64() {
            Some(v) if v > 0.0 => None,
            _ => Some("scheduler: tick_sec must be a positive number of seconds".to_string()),
        }
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let raw_type = payload.get("type").unwrap_or(&Value::Null);
        let verb = raw_type.as_str().unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel),
            "boot" => boot_reply(agent_id, kernel).await,
            "schedule" => schedule_reply(agent_id, payload, kernel).await,
            "unschedule" => unschedule_reply(agent_id, payload, kernel).await,
            "list" => list_reply(agent_id, kernel).await,
            "pause" => pause_reply(agent_id, payload, kernel).await,
            "resume" => resume_reply(agent_id, payload, kernel).await,
            "tick_now" => tick_now_reply(agent_id, payload, kernel).await,
            "history" => history_reply(agent_id, payload, kernel).await,
            // `shutdown` is NOT a verb (A5.3): python's `VERBS` has no such key, so
            // it lands here like any other unknown type. The tick loop dies in
            // `on_delete`, never behind a verb.
            _ => json!({"error": format!("scheduler: unknown type {}", py_repr_value(raw_type))}),
        };
        Ok(Some(reply))
    }

    /// The tick loop and the SCHEDULE CACHE die with the agent.
    /// (A5.14 / D6): python's `_cache` is a module global keyed by agent id and
    /// nothing evicts it, so an agent recreated under the same id in the same
    /// process sees its predecessor's schedules.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        // The cache goes with the agent: a re-created scheduler with the same id
        // used to list and fire the old schedules (python fixed 2026-09-20).
        SCHEDULE_CACHE.lock().remove(agent_id);
        shutdown_tick(agent_id, kernel).await;
        Ok(())
    }
    /// Shutdown frees what delete frees — declared, never a fallback (audit c9).
    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.on_delete(agent_id, kernel).await
    }
}

// ── argument typing ─────────────────────────────────────────────────

// ABSENT ⇒ `default`. Present, an integer, ≥ `minimum` ⇒ that value. ANYTHING
// ELSE (a bool, a string, a float, an explicit null, a value under the bound)
// ⇒ a `bad_arg` refusal.
//
// A present-but-wrong number is a TYPO, and `as_u64().unwrap_or(default)` hid
// it: `interval_seconds: "60"` silently became an hourly schedule the caller
// believed was per-minute, and nothing said so.
fn int_arg(
    payload: &Value,
    key: &str,
    default: u64,
    verb: &str,
    minimum: u64,
) -> Result<u64, Value> {
    let Some(raw) = payload.get(key) else {
        return Ok(default);
    };
    match raw.as_u64() {
        Some(n) if n >= minimum => Ok(n),
        _ => {
            let want = if minimum == 0 {
                "a non-negative int".to_string()
            } else {
                format!("an int >= {minimum}")
            };
            Err(bad(verb, key, &want))
        }
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn meta_value(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<Value> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).cloned()
}

/// The record's sweep interval, python's `_tick_sec`.
///
/// A bool, a non-number or a non-positive value is the DOCUMENTED DEFAULT (the write
/// gate already refused those), and there is NO floor: a tick faster than a tenth of
/// a second is what the operator asked for (A5.12).
fn tick_sec(agent_id: &AgentId, kernel: &Kernel) -> f64 {
    match meta_value(agent_id, kernel, "tick_sec") {
        Some(Value::Number(n)) => n.as_f64().filter(|v| *v > 0.0).unwrap_or(DEFAULT_TICK_SEC),
        _ => DEFAULT_TICK_SEC,
    }
}

/// A meta flag under PYTHON TRUTHINESS (A5.11) — `paused: "no"` is a paused
/// scheduler there, because a non-empty string is true.
fn meta_truthy(agent_id: &AgentId, kernel: &Kernel, key: &str) -> bool {
    meta_value(agent_id, kernel, key).is_some_and(|v| py_truthy(&v))
}

async fn update_meta(agent_id: &AgentId, kernel: &Arc<Kernel>, key: &str, value: Value) {
    let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
        return;
    };
    let mut patch = Map::new();
    patch.insert(key.to_string(), value);
    agent.update_meta(patch);
    // Persist the meta change THROUGH the discovered provider (no-op if none).
    let _ = fantastic_kernel::persistence::persist(kernel, &agent).await;
}

// ── file-agent-routed persistence ───────────────────────────────────

// THE LOADER OWNS THE PATH. This bundle names only a FILENAME; the sidecar
// lands beside the agent's own agent.yaml in whatever store the kernel
// discovered. No provider is named here and no path is computed.
const SCHEDULES_BLOB: &str = "schedules.yaml";
const HISTORY_BLOB: &str = "schedule_events.yaml";

async fn file_read(agent_id: &AgentId, kernel: &Arc<Kernel>, name: &str) -> Option<String> {
    // Operator authority: reading our OWN sidecar is bookkeeping processed from
    // above, not this agent reaching out.
    let reply = fantastic_kernel::send::with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({"type": "load_blob", "agent_id": agent_id.0, "name": name}),
    )]))
    .await
    .remove(0)
    .result;
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
    let reply = fantastic_kernel::send::with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({
            "type": "persist_blob",
            "agent_id": agent_id.0,
            "name": name,
            "content": content,
        }),
    )]))
    .await
    .remove(0)
    .result;
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
    SCHEDULE_CACHE
        .lock()
        .insert(agent_id.clone(), parsed.clone());
    parsed
}

/// Write the schedules out — RAM CACHE FIRST (A5.9).
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
    loop {
        // py reads the record FIRST and returns when it is gone, then sleeps, then
        // checks `paused`.
        if kernel.agents.get(&agent_id).is_none() {
            return; // agent was deleted; exit cleanly
        }
        tokio::time::sleep(Duration::from_secs_f64(tick_sec(&agent_id, &kernel))).await;
        if meta_truthy(&agent_id, &kernel, "paused") {
            continue;
        }
        let now = now_secs();
        let mut schedules = load_schedules(&agent_id, &kernel).await;
        let mut due: Vec<Value> = Vec::new();
        for sch in schedules.iter_mut() {
            if sch.get("paused").is_some_and(py_truthy) {
                continue;
            }
            let next_run = sch.get("next_run").and_then(Value::as_f64).unwrap_or(0.0);
            if now < next_run {
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
        if due.is_empty() {
            continue;
        }
        if let Err(e) = save_schedules(&agent_id, &kernel, &schedules).await {
            // python `logger.warning("scheduler %s: persist failed: %s", sid,
            // err.get("error"))` — an ABSENT `error` key reads `None`.
            log_line(format!(
                "scheduler {agent_id}: persist failed: {}",
                py_str(e.get("error").unwrap_or(&Value::Null))
            ));
        }
        fire_many(&agent_id, &kernel, &due).await;
    }
}

/// A schedule's `target`, python's way: whatever was stored, as text. `schedule`
/// only checks the field for TRUTHINESS, so a non-string target is stored verbatim
/// and fired verbatim (A5.5).
fn sched_target(sch: &Value) -> String {
    match sch.get("target") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_str(other),
        None => String::new(),
    }
}

/// A schedule's `payload` under py `sch.get("payload") or {}`.
fn sched_payload(sch: &Value) -> Value {
    match sch.get("payload").filter(|v| py_truthy(v)) {
        Some(p) => p.clone(),
        None => json!({}),
    }
}

fn fire_item(sch: &Value) -> Item {
    let mut item = Item::new(
        AgentId::from(sched_target(sch).as_str()),
        sched_payload(sch),
    );
    if let Some(t) = sch.get("timeout").and_then(Value::as_f64) {
        item = item.with_timeout(t);
    }
    item
}

/// One fire's `(result, error)` pair for the `schedule_fired` event (A5.8).
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

/// Fire every due schedule as ONE batch send — concurrent, no head-of-line
/// blocking — then record one `schedule_fired` per outcome.
async fn fire_many(agent_id: &AgentId, kernel: &Arc<Kernel>, due: &[Value]) {
    let ts = now_secs();
    let outcomes = kernel.send(due.iter().map(fire_item).collect()).await;
    let duration_ms = ((now_secs() - ts) * 1000.0) as i64;
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
        // ONE delivery per tick (py-canonical, 2026-08-07). The target-echo of
        // `schedule_fired` doubled every delivery to a live program's on_message —
        // the payload send IS the tick; telemetry rides the scheduler's own inbox
        // and schedule_events.yaml.
        kernel.emit(agent_id, event).await;
    }
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
    // SAFETY: `buf` is a valid writable buffer of exactly 3 bytes.
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr().cast(), buf.len()) };
    assert!(rc == 0, "getentropy failed: no randomness source");
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
        "tick_sec": tick_sec(agent_id, kernel),
        "paused": meta_truthy(agent_id, kernel, "paused"),
        "running": running,
        // py `{n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()}`
        // — the FIRST LINE of each verb's docstring, in `VERBS` order. `boot`'s is
        // truncated mid-sentence; that is the python text.
        "verbs": {
            "reflect": "Identity + tick state. No args.",
            "boot": "Idempotent. Starts the tick loop; persistence is THROUGH the loader.",
            "schedule": "args: target:str, payload:{type:..,...}, interval_seconds:int (default 60), timeout:number? (seconds per fire; absent = wait forever). Returns {schedule_id, schedule}. Persisted THROUGH the loader; failfast if no store wired.",
            "unschedule": "args: schedule_id:str (req). Returns {removed:bool, schedule_id}.",
            "list": "No args. Returns {schedules: [<schedule record>, ...]}.",
            "pause": "args: schedule_id:str?. With id pauses one schedule; without, pauses the whole scheduler.",
            "resume": "args: schedule_id:str?. With id resumes one; without, resumes the whole scheduler.",
            "tick_now": "args: schedule_id:str (req). Bumps run_count, persists, then fires that schedule immediately. Returns {fired:true, schedule_id}.",
            "history": "args: limit:int? (1..500, default 100), schedule_id:str?. Returns {history:[event,...], count}.",
        },
        "emits": {
            "schedule_fired": "{type:'schedule_fired', schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} — emitted on the scheduler's OWN inbox after every fire (never the target's — the payload send is the tick)",
        }
    })
}

async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let mut tasks = SCHEDULER_TASKS.lock();
    if let Some(existing) = tasks.get(agent_id) {
        // LIVENESS COMES FROM THE TABLE, which holds only running work.
        if kernel.tasks.contains(existing) {
            // py `return {"running": True}` on BOTH branches (A5.4): boot is
            // idempotent and says nothing else, so a caller cannot branch on
            // whether it was first.
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
        // `catch_unwind`. The traceback body is residual R-D4-8.
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

/// `schedule` — VALIDATION ORDER IS PART OF THE CONTRACT (A5.5): the payload's
/// shape, then `interval_seconds`, then the target, then `payload.type`, then
/// `timeout`. A call with two faults is told about the FIRST one python names.
async fn schedule_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let sched_payload = match payload.get("payload") {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v.clone(),
        // A list/string here can never carry a `type`; say so as an ARG error
        // rather than as the downstream "payload.type required".
        Some(_) => return bad("schedule", "payload", "an object with type"),
    };
    let interval = match int_arg(payload, "interval_seconds", 60, "schedule", 1) {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    // TRUTHINESS, and a truthy non-string target is stored verbatim.
    let Some(target) = payload.get("target").filter(|v| py_truthy(v)).cloned() else {
        return json!({"error": "schedule: target required"});
    };
    if !sched_payload.get("type").is_some_and(py_truthy) {
        return json!({"error": "schedule: payload.type required"});
    }
    // Per-fire bound, seconds. Optional: absent = wait forever (never defaulted).
    // It is STORED AS SENT (A5.6) — an int `5` stays `5`, it does not become `5.0`.
    let timeout = match payload.get("timeout") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) if n.as_f64().is_some_and(|t| t > 0.0) => {
            Some(Value::Number(n.clone()))
        }
        Some(_) => return bad("schedule", "timeout", "a positive number of seconds"),
    };
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
        sch["timeout"] = t;
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
    json!({"schedule_id": sch["id"], "schedule": sch})
}

/// `payload["schedule_id"]` under PYTHON TRUTHINESS (A5.7) — every verb that takes
/// one writes `if not sid:`, and the raw value is echoed back in the reply.
fn schedule_id(payload: &Value) -> Option<&Value> {
    payload.get("schedule_id").filter(|v| py_truthy(v))
}

/// Does this stored schedule carry `sid`? python compares with `==` against the RAW
/// value, so only a string can ever match a stored string id.
fn is_sched(s: &Value, sid: &Value) -> bool {
    s.get("id") == Some(sid)
}

async fn unschedule_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(sid) = schedule_id(payload).cloned() else {
        return json!({"error": "unschedule: schedule_id required"});
    };
    let mut schedules = load_schedules(agent_id, kernel).await;
    let before = schedules.len();
    schedules.retain(|s| !is_sched(s, &sid));
    let removed = schedules.len() < before;
    if removed {
        if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
            return persist_err("unschedule", &e);
        }
    }
    json!({"removed": removed, "schedule_id": sid})
}

async fn list_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let schedules = load_schedules(agent_id, kernel).await;
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
    payload: &Value,
    kernel: &Arc<Kernel>,
    verb: &str,
    flag_key: &str,
    paused: bool,
) -> Value {
    if let Some(sid) = schedule_id(payload).cloned() {
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
        }
        return json!({flag_key: touched > 0, "schedule_id": sid});
    }
    update_meta(agent_id, kernel, "paused", json!(paused)).await;
    json!({flag_key: true, "scheduler_id": agent_id.as_str()})
}

async fn pause_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    set_paused(agent_id, payload, kernel, "pause", "paused", true).await
}

async fn resume_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    set_paused(agent_id, payload, kernel, "resume", "resumed", false).await
}

async fn tick_now_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(sid) = schedule_id(payload).cloned() else {
        return json!({"error": "tick_now: schedule_id required"});
    };
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
    fire_many(agent_id, kernel, &due).await;
    json!({"fired": true, "schedule_id": sid})
}

async fn history_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let limit = match int_arg(payload, "limit", 100, "history", 1) {
        Ok(v) => v.min(500) as usize,
        Err(refusal) => return refusal,
    };
    let mut entries = read_history(agent_id, kernel, limit).await;
    if let Some(sid) = schedule_id(payload) {
        entries.retain(|e| e.get("schedule_id") == Some(sid));
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
