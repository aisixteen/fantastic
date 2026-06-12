//! Recurring-task scheduler as an agent.
//!
//! State (store-relative sidecars `agents/{id}/…` — wire `file_bridge_id` to the
//! `.fantastic` store, so they land next to the agent's own agent.json; routed
//! through that file agent — unset is a hard error at first I/O):
//!
//! - `schedules.json` — `[{id, target, payload, interval_seconds, next_run, paused, run_count, created_at}, …]`
//! - `history.jsonl`  — append-only, one event per fire, ring-trimmed to `HISTORY_MAX`.
//!
//! Verbs (mirror Python verb-for-verb):
//!
//! - `reflect` → `{id, sentence, tick_sec, paused, file_bridge_id, running, verbs, emits}`
//! - `boot`    → idempotent. Starts the tick task. Requires `file_bridge_id`.
//! - `shutdown`→ idempotent. Cancels the tick task.
//! - `schedule`     args `{target, payload, interval_seconds}` → mint + persist
//! - `unschedule`   args `{schedule_id}`                       → remove + persist
//! - `list`                                                    → `{schedules}`
//! - `pause` / `resume` args `{schedule_id?}`                  → flip one or all
//! - `tick_now`     args `{schedule_id}`                       → fire immediately
//! - `history`      args `{limit?, schedule_id?}`              → tail of history.jsonl
//!
//! After every fire the scheduler emits `{type:"schedule_fired", ...}` to
//! its OWN inbox AND the target's inbox.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
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

// ── meta helpers ────────────────────────────────────────────────────

fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

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

// STORE-RELATIVE paths (`agents/<id>/…`): wire `file_bridge_id` to the
// `.fantastic` store and the sidecars land next to the agent's own agent.json —
// one store, no `.fantastic/.fantastic/…` double-nest. Matches Python.
fn schedules_path(agent_id: &AgentId) -> String {
    format!("agents/{}/schedules.json", agent_id)
}

fn history_path(agent_id: &AgentId) -> String {
    format!("agents/{}/history.jsonl", agent_id)
}

async fn file_read(agent_id: &AgentId, kernel: &Arc<Kernel>, path: &str) -> Option<String> {
    let fid = meta_string(agent_id, kernel, "file_bridge_id")?;
    let reply = kernel
        .send(
            &AgentId::from(fid.as_str()),
            json!({"type": "read", "path": path}),
        )
        .await;
    reply
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn file_write(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    path: &str,
    content: &str,
) -> Result<(), String> {
    let fid = match meta_string(agent_id, kernel, "file_bridge_id") {
        Some(s) => s,
        None => return Err("file_bridge_id unset".to_string()),
    };
    let reply = kernel
        .send(
            &AgentId::from(fid.as_str()),
            json!({"type": "write", "path": path, "content": content}),
        )
        .await;
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        return Err(err.to_string());
    }
    Ok(())
}

async fn load_schedules(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Vec<Value> {
    if let Some(cached) = SCHEDULE_CACHE.lock().get(agent_id).cloned() {
        return cached;
    }
    let raw = file_read(agent_id, kernel, &schedules_path(agent_id)).await;
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
) -> Result<(), String> {
    let body = serde_json::to_string_pretty(schedules).map_err(|e| format!("serialize: {e}"))?;
    file_write(agent_id, kernel, &schedules_path(agent_id), &body).await?;
    SCHEDULE_CACHE
        .lock()
        .insert(agent_id.clone(), schedules.to_vec());
    Ok(())
}

async fn append_history(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    event: &Value,
) -> Result<(), String> {
    let prev = file_read(agent_id, kernel, &history_path(agent_id))
        .await
        .unwrap_or_default();
    let line = serde_json::to_string(event).map_err(|e| format!("serialize: {e}"))?;
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
    file_write(agent_id, kernel, &history_path(agent_id), &trimmed).await
}

async fn read_history(agent_id: &AgentId, kernel: &Arc<Kernel>, limit: usize) -> Vec<Value> {
    let raw = match file_read(agent_id, kernel, &history_path(agent_id)).await {
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
        let mut any_fired = false;
        for sch in schedules.iter_mut() {
            if sch.get("paused").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let next_run = sch.get("next_run").and_then(Value::as_f64).unwrap_or(0.0);
            if now < next_run {
                continue;
            }
            fire_schedule(&agent_id, &kernel, sch).await;
            let count = sch.get("run_count").and_then(Value::as_u64).unwrap_or(0);
            let interval = sch
                .get("interval_seconds")
                .and_then(Value::as_f64)
                .unwrap_or(60.0);
            sch["run_count"] = json!(count + 1);
            sch["next_run"] = json!(now_secs() + interval);
            any_fired = true;
        }
        if any_fired {
            if let Err(e) = save_schedules(&agent_id, &kernel, &schedules).await {
                tracing::warn!(error = %e, "scheduler: save after fire failed");
            }
        }
    }
}

async fn fire_schedule(agent_id: &AgentId, kernel: &Arc<Kernel>, sch: &Value) {
    let target = sch
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let payload = sch.get("payload").cloned().unwrap_or(Value::Null);
    let sched_id = sch
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ts = now_secs();
    let mut error: Option<String> = None;
    let mut result = Value::Null;
    if target.is_empty() {
        error = Some("empty target".to_string());
    } else {
        let reply = kernel
            .send(&AgentId::from(target.as_str()), payload.clone())
            .await;
        if let Some(err) = reply.get("error").and_then(Value::as_str) {
            error = Some(err.to_string());
        } else {
            result = reply;
        }
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
        "duration_ms": ((now_secs() - ts) * 1000.0) as i64,
    });
    if let Err(e) = append_history(agent_id, kernel, &event).await {
        tracing::warn!(error = %e, "scheduler: history append failed");
    }
    kernel.emit(agent_id, event.clone()).await;
    if !target.is_empty() && target != agent_id.0 {
        kernel.emit(&AgentId::from(target.as_str()), event).await;
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
    let file_bridge_id = meta_string(agent_id, kernel, "file_bridge_id");
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
        "file_bridge_id": file_bridge_id,
        "running": running,
        "verbs": {
            "reflect": "Identity + tick state + file_bridge_id. No args.",
            "boot": "Idempotent. Starts the tick loop. Requires file_bridge_id.",
            "shutdown": "Idempotent. Cancels the tick loop.",
            "schedule": "args: target:str, payload:dict, interval_seconds:int (default 60).",
            "unschedule": "args: schedule_id:str.",
            "list": "No args. Returns {schedules:[...]}.",
            "pause": "args: schedule_id:str?. Pauses one or all.",
            "resume": "args: schedule_id:str?. Resumes one or all.",
            "tick_now": "args: schedule_id:str. Fires immediately.",
            "history": "args: limit:int?, schedule_id:str?.",
        },
        "emits": {
            "schedule_fired": "{type, schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} broadcast to scheduler's inbox AND target's inbox on every fire",
        }
    })
}

async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    if meta_string(agent_id, kernel, "file_bridge_id").is_none() {
        return json!({"error": "scheduler: file_bridge_id required"});
    }
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
    if meta_string(agent_id, kernel, "file_bridge_id").is_none() {
        return json!({"error": "scheduler: file_bridge_id required"});
    }
    let target = payload
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if target.is_empty() {
        return json!({"error": "schedule: target required"});
    }
    let sched_payload = payload.get("payload").cloned().unwrap_or_else(|| json!({}));
    if sched_payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return json!({"error": "schedule: payload.type required"});
    }
    let interval = payload
        .get("interval_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .max(1);
    let now = now_secs();
    let sch = json!({
        "id": mint_id(),
        "target": target,
        "payload": sched_payload,
        "interval_seconds": interval,
        "created_at": now,
        "next_run": now + interval as f64,
        "run_count": 0,
        "paused": false,
    });
    let mut schedules = load_schedules(agent_id, kernel).await;
    schedules.push(sch.clone());
    if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
        return json!({"error": format!("schedule: persist failed: {e}")});
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
            return json!({"error": format!("unschedule: persist failed: {e}")});
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
                return json!({"error": format!("pause: persist failed: {e}")});
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
                return json!({"error": format!("resume: persist failed: {e}")});
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
    for s in schedules.iter_mut() {
        if s.get("id").and_then(Value::as_str) == Some(sid.as_str()) {
            fire_schedule(agent_id, kernel, s).await;
            let count = s.get("run_count").and_then(Value::as_u64).unwrap_or(0);
            s["run_count"] = json!(count + 1);
            if let Err(e) = save_schedules(agent_id, kernel, &schedules).await {
                return json!({"error": format!("tick_now: persist failed: {e}")});
            }
            return json!({"fired": true, "schedule_id": sid});
        }
    }
    json!({"error": format!("schedule '{sid}' not found")})
}

async fn history_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let mut entries = read_history(agent_id, kernel, limit).await;
    if let Some(sid) = payload.get("schedule_id").and_then(Value::as_str) {
        entries.retain(|e| e.get("schedule_id").and_then(Value::as_str) == Some(sid));
    }
    let n = entries.len();
    json!({"history": entries, "count": n})
}

#[cfg(test)]
mod tests;
