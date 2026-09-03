//! Recurring-task scheduler as an agent.
//!
//! State (store-relative sidecars `agents/{id}/…`, persisted THROUGH the loader
//! via `load_blob`/`persist_blob` — nothing to wire; a write failfasts only when
//! the loader has no store, reads are empty until then):
//!
//! - `schedules.json` — `[{id, target, payload, interval_seconds, next_run, paused, run_count, created_at, timeout?}, …]`
//! - `history.jsonl`  — append-only, one event per fire, ring-trimmed to `HISTORY_MAX`.
//!
//! Verbs (mirror Python verb-for-verb):
//!
//! - `reflect` → `{id, sentence, tick_sec, paused, running, verbs, emits}`
//! - `boot`    → idempotent. Starts the tick task.
//! - `shutdown`→ idempotent. Cancels the tick task.
//! - `schedule`     args `{target, payload, interval_seconds, timeout?}` → mint + persist
//! - `unschedule`   args `{schedule_id}`                       → remove + persist
//! - `list`                                                    → `{schedules}`
//! - `pause` / `resume` args `{schedule_id?}`                  → flip one or all
//! - `tick_now`     args `{schedule_id}`                       → fire immediately
//! - `history`      args `{limit?, schedule_id?}`              → tail of history.jsonl
//!
//! A tick collects every DUE schedule, bumps `run_count`/`next_run`, persists
//! FIRST (a crash inside the fire window cannot re-fire on reboot), then fires
//! them all as ONE `kernel.send([...])` batch — concurrent, so one hung target
//! never blocks the others. `timeout` is the schedule's own per-fire bound in
//! seconds; absent = wait forever (the kernel never defaults it). One
//! `{type:"schedule_fired", ...}` per outcome goes to the scheduler's OWN inbox
//! only — the target gets the payload, which is the tick.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{bad, AgentId, Item, Kernel, Status};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "scheduler.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Maximum history entries kept per scheduler (ring-trim threshold).
/// Matches Python's `HISTORY_MAX`.
pub const HISTORY_MAX: usize = 500;

/// Live tick tasks keyed by scheduler agent id. `boot` populates;
/// `shutdown` / `on_delete` abort.
static SCHEDULER_TASKS: OnceLockTaskMap = OnceLockTaskMap::new();

/// In-process schedule cache to avoid re-reading schedules.json on every
/// tick. The on-disk file is the truth; this is a warm copy.
static SCHEDULE_CACHE: OnceLockScheduleCache = OnceLockScheduleCache::new();

// ── once-lock map helpers ───────────────────────────────────────────

struct OnceLockTaskMap(OnceLock<Mutex<HashMap<AgentId, JoinHandle<()>>>>);
impl OnceLockTaskMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, JoinHandle<()>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("SCHEDULER_TASKS poisoned")
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
            .expect("SCHEDULE_CACHE poisoned")
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
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel),
            "boot" => boot_reply(agent_id, kernel).await,
            "shutdown" => shutdown_reply(agent_id),
            "schedule" => schedule_reply(agent_id, payload, kernel).await,
            "unschedule" => unschedule_reply(agent_id, payload, kernel).await,
            "list" => list_reply(agent_id, kernel).await,
            "pause" => pause_reply(agent_id, payload, kernel).await,
            "resume" => resume_reply(agent_id, payload, kernel).await,
            "tick_now" => tick_now_reply(agent_id, payload, kernel).await,
            "history" => history_reply(agent_id, payload, kernel).await,
            other => json!({"error": format!("scheduler: unknown type '{other}'")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let _ = shutdown_reply(agent_id);
        SCHEDULE_CACHE.lock().remove(agent_id);
        Ok(())
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

fn meta_f64(agent_id: &AgentId, kernel: &Kernel, key: &str, default: f64) -> f64 {
    let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
        return default;
    };
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_f64).unwrap_or(default)
}

fn meta_bool(agent_id: &AgentId, kernel: &Kernel, key: &str) -> bool {
    let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
        return false;
    };
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_bool).unwrap_or(false)
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
// lands beside the agent's own agent.json in whatever store the kernel
// discovered. No provider is named here and no path is computed.
const SCHEDULES_BLOB: &str = "schedules.json";
const HISTORY_BLOB: &str = "history.jsonl";

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
    let parsed: Vec<Value> = match raw {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => Vec::new(),
    };
    SCHEDULE_CACHE
        .lock()
        .insert(agent_id.clone(), parsed.clone());
    parsed
}

async fn save_schedules(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    schedules: &[Value],
) -> Result<(), PersistError> {
    let body = serde_json::to_string_pretty(schedules)
        .map_err(|e| json!({"error": format!("serialize: {e}")}))?;
    file_write(agent_id, kernel, SCHEDULES_BLOB, &body).await?;
    SCHEDULE_CACHE
        .lock()
        .insert(agent_id.clone(), schedules.to_vec());
    Ok(())
}

async fn append_history(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    event: &Value,
) -> Result<(), PersistError> {
    let prev = file_read(agent_id, kernel, HISTORY_BLOB)
        .await
        .unwrap_or_default();
    let line =
        serde_json::to_string(event).map_err(|e| json!({"error": format!("serialize: {e}")}))?;
    let mut combined = prev;
    combined.push_str(&line);
    combined.push('\n');
    // Ring-trim past 2× MAX.
    let lines: Vec<&str> = combined.split_terminator('\n').collect();
    let trimmed = if lines.len() > 2 * HISTORY_MAX {
        let keep: Vec<&str> = lines
            .iter()
            .rev()
            .take(HISTORY_MAX)
            .rev()
            .copied()
            .collect();
        keep.join("\n") + "\n"
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
    raw.lines()
        .rev()
        .take(limit)
        .filter_map(|l| {
            let t = l.trim();
            if t.is_empty() {
                None
            } else {
                serde_json::from_str(t).ok()
            }
        })
        .collect::<Vec<Value>>()
        .into_iter()
        .rev()
        .collect()
}

// ── tick loop ───────────────────────────────────────────────────────

async fn tick_loop(agent_id: AgentId, kernel: Arc<Kernel>) {
    loop {
        let tick_sec = meta_f64(&agent_id, &kernel, "tick_sec", 1.0).max(0.1);
        tokio::time::sleep(Duration::from_secs_f64(tick_sec)).await;
        if meta_bool(&agent_id, &kernel, "paused") {
            continue;
        }
        if kernel.agents.get(&agent_id).is_none() {
            return; // agent was deleted; exit cleanly
        }
        let now = now_secs();
        let mut schedules = load_schedules(&agent_id, &kernel).await;
        let mut due: Vec<Value> = Vec::new();
        for sch in schedules.iter_mut() {
            if sch.get("paused").and_then(Value::as_bool).unwrap_or(false) {
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
            tracing::warn!(error = %e, "scheduler: persist failed");
        }
        fire_many(&agent_id, &kernel, &due).await;
    }
}

fn fire_item(sch: &Value) -> Item {
    let target = sch.get("target").and_then(Value::as_str).unwrap_or("");
    let payload = sch.get("payload").cloned().unwrap_or_else(|| json!({}));
    let mut item = Item::new(AgentId::from(target), payload);
    if let Some(t) = sch.get("timeout").and_then(Value::as_f64) {
        item = item.with_timeout(t);
    }
    item
}

/// Fire every due schedule as ONE batch send — concurrent, no head-of-line
/// blocking — then record one `schedule_fired` per outcome.
async fn fire_many(agent_id: &AgentId, kernel: &Arc<Kernel>, due: &[Value]) {
    let ts = now_secs();
    let outcomes = kernel.send(due.iter().map(fire_item).collect()).await;
    let duration_ms = ((now_secs() - ts) * 1000.0) as i64;
    for (sch, out) in due.iter().zip(outcomes) {
        let target = sch.get("target").and_then(Value::as_str).unwrap_or("");
        let payload = sch.get("payload").cloned().unwrap_or_else(|| json!({}));
        let sched_id = sch.get("id").and_then(Value::as_str).unwrap_or("");
        let mut error: Option<String> = None;
        let mut result = Value::Null;
        if out.status != Status::Completed {
            let detail = out
                .result
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| out.result.to_string());
            error = Some(format!("{}: {detail}", out.status));
        } else if let Some(err) = out.result.get("error").and_then(Value::as_str) {
            error = Some(err.to_string());
        } else {
            result = out.result;
        }
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
            tracing::warn!(error = %e, "scheduler: history append failed");
        }
        // ONE delivery per tick (py-canonical, 2026-08-07). The target-echo of
        // `schedule_fired` doubled every delivery to a live program's on_message —
        // the payload send IS the tick; telemetry rides the scheduler's own inbox
        // and history.jsonl.
        kernel.emit(agent_id, event).await;
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn mint_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let mix = nanos ^ stack_ptr ^ std::process::id() as u64;
    format!("sch_{:06x}", (mix as u32) & 0xff_ffff)
}

// ── verb implementations ────────────────────────────────────────────

fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let running = SCHEDULER_TASKS
        .lock()
        .get(agent_id)
        .map(|t| !t.is_finished())
        .unwrap_or(false);
    json!({
        "id": agent_id.as_str(),
        "sentence": "Recurring-task scheduler.",
        "tick_sec": meta_f64(agent_id, kernel, "tick_sec", 1.0),
        "paused": meta_bool(agent_id, kernel, "paused"),
        "running": running,
        "verbs": {
            "reflect": "Identity + tick state. No args.",
            "boot": "Idempotent. Starts the tick loop. Persistence is THROUGH the loader; a write failfasts only when no store is wired.",
            "shutdown": "Idempotent. Cancels the tick loop.",
            "schedule": "args: target:str, payload:dict, interval_seconds:int (default 60), timeout:number? (seconds per fire; absent = wait forever).",
            "unschedule": "args: schedule_id:str.",
            "list": "No args. Returns {schedules:[...]}.",
            "pause": "args: schedule_id:str?. Pauses one or all.",
            "resume": "args: schedule_id:str?. Resumes one or all.",
            "tick_now": "args: schedule_id:str. Bumps run_count, persists, then fires immediately.",
            "history": "args: limit:int?, schedule_id:str?.",
        },
        "emits": {
            "schedule_fired": "{type, schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} emitted on the scheduler's OWN inbox on every fire (never the target's)",
        }
    })
}

async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let mut tasks = SCHEDULER_TASKS.lock();
    if let Some(existing) = tasks.get(agent_id) {
        if !existing.is_finished() {
            return json!({"running": true, "already_booted": true});
        }
    }
    let id = agent_id.clone();
    let k = Arc::clone(kernel);
    let task = tokio::spawn(async move {
        tick_loop(id, k).await;
    });
    tasks.insert(agent_id.clone(), task);
    json!({"running": true})
}

fn shutdown_reply(agent_id: &AgentId) -> Value {
    let removed = SCHEDULER_TASKS.lock().remove(agent_id);
    if let Some(task) = removed {
        task.abort();
        json!({"stopped": true, "id": agent_id.as_str()})
    } else {
        json!({"stopped": false, "id": agent_id.as_str(), "reason": "not running"})
    }
}

async fn schedule_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let target = payload
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if target.is_empty() {
        return json!({"error": "schedule: target required"});
    }
    let sched_payload = match payload.get("payload") {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v.clone(),
        // A list/string here can never carry a `type`; say so as an ARG error
        // rather than as the downstream "payload.type required".
        Some(_) => return bad("schedule", "payload", "an object with type"),
    };
    if sched_payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return json!({"error": "schedule: payload.type required"});
    }
    let interval = match int_arg(payload, "interval_seconds", 60, "schedule", 1) {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    // Per-fire bound, seconds. Optional: absent = wait forever (never defaulted).
    let timeout = match payload.get("timeout") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(t) if t > 0.0 => Some(t),
            _ => return bad("schedule", "timeout", "a positive number of seconds"),
        },
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
        sch["timeout"] = json!(t);
    }
    let mut schedules = load_schedules(agent_id, kernel).await;
    schedules.push(sch.clone());
    if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
        // The cache was not touched (save_schedules only warms it on success),
        // so a failed persist leaves no phantom schedule.
        return persist_err("schedule", &e);
    }
    json!({"schedule_id": sch["id"], "schedule": sch})
}

async fn unschedule_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let sid = match payload.get("schedule_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return json!({"error": "unschedule: schedule_id required"}),
    };
    let mut schedules = load_schedules(agent_id, kernel).await;
    let before = schedules.len();
    schedules.retain(|s| s.get("id").and_then(Value::as_str) != Some(sid.as_str()));
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
    json!({"schedules": schedules})
}

async fn pause_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if let Some(sid) = payload.get("schedule_id").and_then(Value::as_str) {
        let mut schedules = load_schedules(agent_id, kernel).await;
        let mut touched = 0;
        for s in schedules.iter_mut() {
            if s.get("id").and_then(Value::as_str) == Some(sid) {
                s["paused"] = json!(true);
                touched += 1;
            }
        }
        if touched > 0 {
            if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
                return persist_err("pause", &e);
            }
        }
        return json!({"paused": touched > 0, "schedule_id": sid});
    }
    update_meta(agent_id, kernel, "paused", json!(true)).await;
    json!({"paused": true, "scheduler_id": agent_id.as_str()})
}

async fn resume_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if let Some(sid) = payload.get("schedule_id").and_then(Value::as_str) {
        let mut schedules = load_schedules(agent_id, kernel).await;
        let mut touched = 0;
        for s in schedules.iter_mut() {
            if s.get("id").and_then(Value::as_str) == Some(sid) {
                s["paused"] = json!(false);
                touched += 1;
            }
        }
        if touched > 0 {
            if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
                return persist_err("resume", &e);
            }
        }
        return json!({"resumed": touched > 0, "schedule_id": sid});
    }
    update_meta(agent_id, kernel, "paused", json!(false)).await;
    json!({"resumed": true, "scheduler_id": agent_id.as_str()})
}

async fn tick_now_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let sid = match payload.get("schedule_id").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return json!({"error": "tick_now: schedule_id required"}),
    };
    let mut schedules = load_schedules(agent_id, kernel).await;
    let Some(idx) = schedules
        .iter()
        .position(|s| s.get("id").and_then(Value::as_str) == Some(sid.as_str()))
    else {
        return json!({"error": format!("schedule '{sid}' not found")});
    };
    // Same order as the tick: bump → persist → fire.
    let count = schedules[idx]
        .get("run_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    schedules[idx]["run_count"] = json!(count + 1);
    if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
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
    if let Some(sid) = payload.get("schedule_id").and_then(Value::as_str) {
        entries.retain(|e| e.get("schedule_id").and_then(Value::as_str) == Some(sid));
    }
    let n = entries.len();
    json!({"history": entries, "count": n})
}

#[cfg(test)]
mod tests;
