//! Per-agent coordination state, keyed by agent id in a process-global
//! map. State is shared across backends — an ollama agent's id never
//! collides with a NIM agent's id, so both coexist safely. The provider
//! (built per agent) is the only per-backend seam.

use crate::helpers::{now_secs, safe_client};
use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// One queued entry awaiting the FIFO lock.
#[derive(Clone, Debug)]
pub struct QueuedEntry {
    /// Sanitised caller client id.
    pub client_id: String,
    /// The user text for this submission.
    pub text: String,
    /// Correlation id for status events.
    pub send_id: String,
    /// Enqueue timestamp.
    pub queued_at: f64,
}

/// The entry currently holding the FIFO lock (used by `status`).
#[derive(Clone, Debug)]
pub struct CurrentEntry {
    /// Sanitised caller client id.
    pub client_id: String,
    /// The user text for this submission.
    pub text: String,
    /// Correlation id for status events.
    pub send_id: String,
    /// Lock-acquisition timestamp.
    pub started_at: f64,
    /// Current phase: thinking|streaming|tool_calling|done.
    pub phase: String,
    /// Accumulated streamed text (for status snapshots).
    pub text_so_far: String,
    /// Last tool entry (entry/exit), for status snapshots.
    pub last_tool: Option<Value>,
}

/// Per-agent coordination state. Cloning the `Arc` is cheap.
pub struct BackendState {
    /// FIFO serializer — only one `send` runs at a time per agent.
    pub lock: Arc<AsyncMutex<()>>,
    /// In-flight task handle. `interrupt` aborts via this.
    pub current_task: Mutex<Option<JoinHandle<()>>>,
    /// Snapshot for the `status` verb. `Some` iff a `send` holds the lock.
    pub current_meta: Mutex<Option<CurrentEntry>>,
    /// Entries waiting on `lock`. Front = next to acquire.
    pub queue: Mutex<VecDeque<QueuedEntry>>,
    /// Lazy menu cache — `None` means "rebuild on next assemble".
    pub menu: Mutex<Option<Vec<Value>>>,
}

impl BackendState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lock: Arc::new(AsyncMutex::new(())),
            current_task: Mutex::new(None),
            current_meta: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            menu: Mutex::new(None),
        })
    }
}

struct OnceLockBackends(OnceLock<Mutex<HashMap<AgentId, Arc<BackendState>>>>);
impl OnceLockBackends {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Arc<BackendState>>> {
        self.0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("BACKENDS poisoned")
    }
}

static BACKENDS: OnceLockBackends = OnceLockBackends::new();

/// Get (or lazily create) the state for an agent.
pub fn state_for(agent_id: &AgentId) -> Arc<BackendState> {
    let mut map = BACKENDS.lock();
    Arc::clone(
        map.entry(agent_id.clone())
            .or_insert_with(BackendState::new),
    )
}

/// `true` iff a `send` task is registered and not finished.
pub fn is_generating(agent_id: &AgentId) -> bool {
    BACKENDS
        .lock()
        .get(agent_id)
        .map(|s| {
            s.current_task
                .lock()
                .expect("task poisoned")
                .as_ref()
                .map(|t| !t.is_finished())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Abort any in-flight task and drop the agent's state slot.
pub fn drop_state(agent_id: &AgentId) {
    let state = BACKENDS.lock().remove(agent_id);
    if let Some(s) = state {
        if let Some(task) = s.current_task.lock().expect("task poisoned").take() {
            task.abort();
        }
    }
}

/// Abort any in-flight task (does NOT drop the state slot). Returns
/// `true` iff a live task was aborted.
pub fn interrupt(agent_id: &AgentId) -> bool {
    let task_opt = {
        let map = BACKENDS.lock();
        map.get(agent_id)
            .and_then(|s| s.current_task.lock().expect("task poisoned").take())
    };
    if let Some(task) = task_opt {
        if !task.is_finished() {
            task.abort();
            return true;
        }
    }
    false
}

// ── status snapshot ─────────────────────────────────────────────────

fn redact_entry(c: &CurrentEntry, requesting: Option<&str>) -> Value {
    let is_mine = requesting
        .map(|r| r == c.client_id.as_str())
        .unwrap_or(false);
    let elapsed = (now_secs() - c.started_at).max(0.0);
    let mut out = json!({
        "client_id": c.client_id,
        "send_id": c.send_id,
        "started_at": c.started_at,
        "phase": c.phase,
        "elapsed": elapsed,
        "is_mine": is_mine,
    });
    if is_mine {
        let obj = out.as_object_mut().unwrap();
        obj.insert("text".to_string(), json!(c.text));
        obj.insert("text_so_far".to_string(), json!(c.text_so_far));
        if let Some(t) = &c.last_tool {
            obj.insert("last_tool".to_string(), t.clone());
        }
    }
    out
}

/// Build the `status` verb reply (privacy-filtered).
pub fn status_snapshot(agent_id: &AgentId, payload: &Value) -> Value {
    let requesting = payload
        .get("client_id")
        .and_then(Value::as_str)
        .map(safe_client);
    let state = state_for(agent_id);
    let cur = state.current_meta.lock().expect("current poisoned").clone();
    let queue = state
        .queue
        .lock()
        .expect("queue poisoned")
        .iter()
        .cloned()
        .collect::<Vec<_>>();

    let current_out = cur
        .as_ref()
        .map(|c| redact_entry(c, requesting.as_deref()))
        .unwrap_or(Value::Null);

    let (mine_pending, others_pending) = match &requesting {
        Some(req) => {
            let mut mine = Vec::new();
            let mut others = 0u64;
            for q in queue.iter() {
                if q.client_id == *req {
                    mine.push(json!({
                        "send_id": q.send_id,
                        "text": q.text,
                        "queued_at": q.queued_at,
                    }));
                } else {
                    others += 1;
                }
            }
            (Value::Array(mine), others)
        }
        None => (Value::Array(Vec::new()), queue.len() as u64),
    };

    json!({
        "source": agent_id.as_str(),
        "client_id": requesting,
        "generating": cur.is_some(),
        "current": current_out,
        "mine_pending": mine_pending,
        "others_pending": others_pending,
    })
}
