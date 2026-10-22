//! Per-agent coordination state, keyed by agent id in a process-global
//! map. State is shared across backends — an ollama agent's id never
//! collides with a vllm agent's id, so both coexist safely. The provider
//! (built per agent) is the only per-backend seam.

use crate::helpers::{now_secs, safe_client};
use fantastic_bundle::py_truthy;
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
    /// This layer's own text to its own agent (a reminder, a malformed-call
    /// report): the incoming row is stored as `kind: "notice"`.
    pub notice: bool,
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
    /// Enqueue timestamp, carried over from the queue row — python promotes the
    /// WHOLE queued entry (`{**entry, started_at, phase, …}`), so `queued_at`
    /// survives into `status.current` and says how long the caller waited.
    pub queued_at: f64,
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
    /// This pass's STREAMING PARTS — the splitter + publisher the stream loop
    /// feeds (python `_current[id]["parts"]`). `stop` checks a `stream_id`
    /// against it; `on_cancel` aborts its open parts. `None` until the provider
    /// pass starts.
    pub parts: Option<Arc<AsyncMutex<crate::events::PassParts>>>,
    /// Fired by `stop`: the generation's own loop watches it and drops the
    /// provider stream, so a stop reaches the stream whether or not the kernel's
    /// task walk does (#177).
    pub abort: Arc<tokio::sync::Notify>,
    /// Who stopped this generation with the `stop` verb — the caller's id, or
    /// `"operator"` (python `stopped_by`). The cut marker names them.
    pub stopped_by: Option<String>,
    /// Carried from the queue row: the incoming row is a `notice`.
    pub notice: bool,
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
    /// verb's `last_projection`). `{fired, strategy?, kept_messages?,
    /// dropped_messages?, summarized?, too_small?}`. Free of internal bookkeeping.
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
    /// TOKEN CALIBRATION — python `_calib`: the prompt size the endpoint
    /// REPORTED for the last pass divided by our own estimate of what we sent.
    /// The estimate is chars/4 over the rendered messages; a real tokenizer
    /// disagrees with it, per model and per language. The budget is measured in
    /// ESTIMATE units scaled by this ratio, so what triggers a compaction is what
    /// the window really holds. `None` (read as 1.0) until an endpoint reports.
    pub calib: Mutex<Option<f64>>,
    /// The endpoint's last reported prompt size — python `_reported`.
    pub reported: Mutex<Option<i64>>,
    /// ONE CHAIN PER AGENT (#227): whether a chain is running, the token its
    /// own turns carry, and the client_ids of messages held for its next pass.
    pub chain: Mutex<ChainState>,
}

/// The agent's chain: `alive` while a pass or its routing is running.
#[derive(Default)]
pub struct ChainState {
    /// A pass or its routing is running.
    pub alive: bool,
    /// The token this chain's own turns carry.
    pub token: String,
    /// Messages held for the chain's next pass, in order. Each holds the
    /// oneshot its caller awaits, so a held send returns the answer of the
    /// pass that reads it.
    pub held: Vec<HeldMsg>,
}

/// A message held while its agent's chain is alive: its client_id, and the
/// channel that hands its caller the reply of the pass that answers it.
pub struct HeldMsg {
    /// The held message's client_id.
    pub client_id: String,
    /// Hands the caller the reply of the pass that answers it.
    pub reply: tokio::sync::oneshot::Sender<serde_json::Value>,
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
            calib: Mutex::new(None),
            reported: Mutex::new(None),
            chain: Mutex::new(ChainState::default()),
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

/// How many times this agent has BOOTED in this process — python
/// `_incarnation`. A delivery captures it when it is decided; a reply that
/// crosses the next boot answered a call the previous incarnation made.
fn incarnations() -> &'static Mutex<HashMap<AgentId, u64>> {
    static INCARNATIONS: OnceLock<Mutex<HashMap<AgentId, u64>>> = OnceLock::new();
    INCARNATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// This agent's current incarnation (0 before its first boot).
#[must_use]
pub fn incarnation(agent_id: &AgentId) -> u64 {
    *incarnations()
        .lock()
        .expect("incarnations poisoned")
        .get(agent_id)
        .unwrap_or(&0)
}

/// A boot: the agent is a NEW incarnation from here on. Every backend's `boot`
/// verb calls this first, exactly as python's `_make_boot` does.
pub fn bump_incarnation(agent_id: &AgentId) {
    let mut m = incarnations().lock().expect("incarnations poisoned");
    *m.entry(agent_id.clone()).or_insert(0) += 1;
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

/// The `current` block of a status snapshot — python `_status_snapshot`'s two
/// shapes, which are NOT the same dict with a flag.
///
/// WITH a requesting client the block is BUILT: `{phase, send_id, started_at,
/// elapsed, is_mine}`, plus this caller's own text when the turn is theirs.
/// WITHOUT one it is the in-flight ENTRY with its text-bearing keys removed
/// (`_redact_text`), so it keeps `client_id` and `queued_at` and carries the
/// entry's own key order. Rust answered one shape for both, with `client_id`
/// leaked into the per-caller block and `queued_at` missing from the other.
fn current_out(c: &CurrentEntry, requesting: Option<&str>) -> Value {
    let elapsed = now_secs() - c.started_at;
    let Some(req) = requesting else {
        return json!({
            "client_id": c.client_id,
            "send_id": c.send_id,
            "queued_at": c.queued_at,
            "started_at": c.started_at,
            "phase": c.phase,
            "elapsed": elapsed,
            "is_mine": false,
        });
    };
    let is_mine = req == c.client_id;
    let mut out = json!({
        "phase": c.phase,
        "send_id": c.send_id,
        "started_at": c.started_at,
        "elapsed": elapsed,
        "is_mine": is_mine,
    });
    if is_mine {
        let obj = out.as_object_mut().expect("object literal");
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
    // PYTHON TRUTHINESS DECIDES, and the reply ECHOES what it decided on:
    // `cid = payload.get("client_id"); if cid: cid = _safe_client(cid)`. An
    // EMPTY string is falsy — it takes the redacted branch and comes back as
    // `''`. Rust ran `safe_client` unconditionally, so `''` became `'cli'` and a
    // caller asking for the redacted view was answered as if it were `cli`.
    let raw_client = payload.get("client_id").cloned().unwrap_or(Value::Null);
    let requesting = if py_truthy(&raw_client) {
        // `str(client_id)` for a truthy non-string (python).
        Some(safe_client(&fantastic_bundle::py_str(&raw_client)))
    } else {
        None
    };
    let echoed_client = match &requesting {
        Some(c) => json!(c),
        None => raw_client,
    };
    let state = state_for(agent_id);
    let cur = state.current_meta.lock().expect("current poisoned").clone();
    let queue = state
        .queue
        .lock()
        .expect("queue poisoned")
        .iter()
        .cloned()
        .collect::<Vec<_>>();

    let current = cur
        .as_ref()
        .map(|c| current_out(c, requesting.as_deref()))
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
        "client_id": echoed_client,
        "generating": cur.is_some(),
        "current": current,
        "mine_pending": mine_pending,
        "others_pending": others_pending,
    })
}
