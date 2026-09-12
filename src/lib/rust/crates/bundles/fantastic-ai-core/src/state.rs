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
    /// The KERNEL TASK this generation runs as (`"<agent>:<n>"`) — the pid of
    /// the `send` dispatch. `cancel <task_id>` kills the turn; the bundle's
    /// `on_cancel` matches on this to know the killed task is ITS generation
    /// (not some other dispatch on the same agent) and to close the turn out.
    pub task_id: Option<String>,
}

/// Per-agent coordination state. Cloning the `Arc` is cheap.
pub struct BackendState {
    /// FIFO serializer — only one `send` runs at a time per agent.
    pub lock: Arc<AsyncMutex<()>>,
    /// Snapshot for the `status` verb. `Some` iff a `send` holds the lock.
    pub current_meta: Mutex<Option<CurrentEntry>>,
    /// Entries waiting on `lock`. Front = next to acquire.
    pub queue: Mutex<VecDeque<QueuedEntry>>,
    /// PUBLIC last context-overflow projection summary (the `context_status`
    /// verb's `last_projection`). `{fired, strategy?, kept_turns?,
    /// dropped_turns?, summarized?, too_small?}`. Free of internal bookkeeping.
    pub projection: Mutex<Option<Value>>,
    /// PRIVATE reaction cursor: `(fired_at_index, client_id)` where the last
    /// compaction notice landed in the durable store, so `derive_reaction`
    /// can scan from there. Never surfaced in a verb reply.
    pub compaction_mark: Mutex<Option<(usize, String)>>,
    /// Fold rows written since the last automatic `context_recall` — python
    /// `_folds_since`. Every `RECALL_EVERY`-th batch marks a recall due.
    pub folds_since: Mutex<usize>,
    /// A `context_recall` is owed at the start of the next turn — python
    /// `_recall_due`. "When it feels it folded, it runs context_recall": at the
    /// start of the NEXT turn, over the new head. Debounced, because a recall is
    /// a model call and one after every fold batch doubled the turn price.
    pub recall_due: Mutex<bool>,
    /// Where the last assembled body came from — `"array"` (the view array),
    /// `"store"` (no view yet) or `"store (stale array)"`. Python `_view_source`,
    /// reported by `context_status`.
    pub view_source: Mutex<Option<String>>,
}

impl BackendState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lock: Arc::new(AsyncMutex::new(())),
            current_meta: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            projection: Mutex::new(None),
            compaction_mark: Mutex::new(None),
            folds_since: Mutex::new(0),
            recall_due: Mutex::new(false),
            view_source: Mutex::new(None),
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

/// `true` iff a `send` holds the FIFO lock right now (a generation is in
/// flight). Read off `current_meta` — the backend keeps no task handle of its
/// own: a generation IS its kernel dispatch task (see the `ps`/`cancel` system
/// verbs), and the kernel owns the abort.
pub fn is_generating(agent_id: &AgentId) -> bool {
    BACKENDS
        .lock()
        .get(agent_id)
        .map(|s| s.current_meta.lock().expect("current poisoned").is_some())
        .unwrap_or(false)
}

/// Drop the agent's state slot (delete / shutdown). An in-flight `send` is a
/// KERNEL task — it dies with the agent's dispatches, not through a bundle-held
/// handle.
pub fn drop_state(agent_id: &AgentId) {
    BACKENDS.lock().remove(agent_id);
}

/// Close out a generation the kernel is cancelling: if `task_id` names the turn
/// currently holding the lock, clear the in-flight entry and hand back the
/// client id so the backend can emit its final `done` (the `on_cancel` half of
/// the task table's two-phase kill). `None` if the cancelled task isn't this
/// agent's generation (a `history` / `reflect` dispatch on the same agent).
pub fn take_current_for_cancel(agent_id: &AgentId, task_id: &str) -> Option<CurrentEntry> {
    let state = BACKENDS.lock().get(agent_id).map(Arc::clone)?;
    let mut cur = state.current_meta.lock().expect("current poisoned");
    if cur.as_ref()?.task_id.as_deref() != Some(task_id) {
        return None;
    }
    cur.take()
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
