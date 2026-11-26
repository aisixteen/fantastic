//! Per-agent coordination state, keyed by agent id in a process-global
//! map. State is shared across backends — an ollama agent's id never
//! collides with a vllm agent's id, so both coexist safely. The provider
//! (built per agent) is the only per-backend seam.

use crate::helpers::{now_secs, safe_client};
use fantastic_kernel::AgentId;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;

/// The pass in flight (used by `status`, `stop` and `on_cancel`).
#[derive(Clone, Debug)]
pub struct CurrentEntry {
    /// Sanitised caller client id.
    pub client_id: String,
    /// The user text for this submission.
    pub text: String,
    /// Correlation id for status events.
    pub send_id: String,
    /// When the first message this pass reads arrived.
    pub queued_at: f64,
    /// The sender's id (the calling agent, else the client): the `source` of the
    /// incoming row.
    pub origin: String,
    /// When the pass started.
    pub started_at: f64,
    /// Current phase of the call: queued|thinking|streaming|retrying, or the phase that ended it.
    pub phase: String,
    /// Accumulated streamed text (for status snapshots).
    pub text_so_far: String,
    /// Last tool entry (entry/exit), for status snapshots.
    pub last_tool: Option<Value>,
    /// The KERNEL EXECUTION of the pass's request in flight: the provider's
    /// `complete` call, or the chat's own task that reads an in-process provider.
    /// `stop` cancels it; the bundle's `on_cancel` matches on it to know the killed
    /// task is ITS pass's request (not some other dispatch on the same agent).
    pub task_id: Option<String>,
    /// This pass's STREAMING PARTS — the splitter + publisher the stream loop
    /// feeds (python `_current[id]["parts"]`). `stop` checks a `stream_id`
    /// against it; `on_cancel` aborts its open parts. `None` until the provider
    /// pass starts.
    pub parts: Option<Arc<AsyncMutex<crate::events::PassParts>>>,
    /// Fired by `stop`: an in-process provider's read watches it and drops the
    /// stream, so a stop reaches the stream whether or not the kernel's task walk
    /// does (#177).
    pub abort: Arc<tokio::sync::Notify>,
    /// Who stopped this generation with the `stop` verb — the caller's id, or
    /// `"operator"` (python `stopped_by`). The cut marker names them.
    pub stopped_by: Option<String>,
    /// The pass reads only this layer's own text (a notice).
    pub notice: bool,
    /// The row the pass answers: the newest it reads, its answer row's parent.
    pub(crate) answers: i64,
    /// The number of the pass's request in flight: a request id recorded late is not
    /// taken for a newer one's.
    pub(crate) request: u64,
}

/// Per-agent coordination state. Cloning the `Arc` is cheap.
pub struct BackendState {
    /// Snapshot for the `status` verb. `Some` iff a pass is in flight.
    pub current_meta: Mutex<Option<CurrentEntry>>,
    /// THE AGENT'S LIFE (#795): whether a pass runs, the messages stored and not yet
    /// read by one, and the calls its answers have in flight.
    pub(crate) life: Mutex<Life>,
    /// Told whenever [`Life`] changes: a test waits on it for the agent to be quiet.
    pub(crate) life_changed: tokio::sync::Notify,
    /// How this agent's passes reach their model: set by every call the bundle
    /// answers, read by a pass that starts outside a call (a reply's wake, the next
    /// pass over the messages a pass did not read).
    pub(crate) machine: Mutex<Option<Machine>>,
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
    /// What the previous request of this chat carried, for the next row's `parts`
    /// delta and running totals; `None` on a fresh start and after a compaction.
    pub(crate) prev_request: Mutex<Option<crate::row_attrs::PrevRequest>>,
    /// TOKEN CALIBRATION — python `_calib`: the prompt size the endpoint
    /// REPORTED for the last pass divided by our own estimate of what we sent.
    /// The estimate is chars/4 over the rendered messages; a real tokenizer
    /// disagrees with it, per model and per language. The budget is measured in
    /// ESTIMATE units scaled by this ratio, so what triggers a compaction is what
    /// the window really holds. `None` (read as 1.0) until an endpoint reports.
    pub calib: Mutex<Option<f64>>,
    /// The endpoint's last reported prompt size — python `_reported`.
    pub reported: Mutex<Option<i64>>,
    /// When the last provider pass ended (unix seconds) — `fork_points` reads it
    /// to say whether the prompt cache is still warm. Python `_last_send`.
    pub last_send_secs: Mutex<Option<f64>>,
    /// The provider and model the chat's `provider_capabilities` were last checked for in
    /// this incarnation: the provider is asked again at the first turn after a boot and
    /// whenever either changes (#622).
    pub caps_checked: Mutex<Option<(String, String)>>,
    /// Image rows a `recall` brought back: shown whole in the next request, once.
    pub recalled_images: Mutex<Vec<i64>>,
    /// The chat's current one-line summary `(text, ts)`: its latest `line_summary` call
    /// ([`crate::line_summary`]), restored from its history at boot.
    pub line_summary: Mutex<Option<(String, f64)>>,
    /// The model the chat's provider last named (#766): the `model` of a pass's reply and
    /// of a `capabilities` answer. RAM only; the second source of the model id a pass's
    /// call format is chosen by, cleared when the record's `(provider_id, model)` changes.
    pub(crate) model_seen: Mutex<Option<String>>,
    /// The record's `(provider_id, model)` pair the model id was last resolved for, with
    /// the provider's `models` default once asked for that pair (#766).
    pub(crate) model_key: Mutex<Option<crate::model_format::rules::ModelKey>>,
    /// The chat's own RENDER CACHE (#766): its rows rendered for the model type, RAM
    /// only, bounded by its record's `render_cache_bytes`. It goes with this state.
    pub(crate) render_cache: Arc<crate::model_format::cache::RenderCache>,
    /// The chat's token counts since its first pass (#766, 1.7): RAM, `None` until seeded
    /// from the newest assistant row ([`crate::used::seed`]).
    pub(crate) used_running: Mutex<Option<crate::used::Used>>,
    /// Token counts with no row yet (#766, 1.7): they land on the next stored assistant
    /// row ([`crate::used::land`]); a process that dies loses them.
    pub(crate) used_pending: Mutex<crate::used::Pending>,
    /// The model type of the chat's last finished turn pass (#766, 1.7): the calibration
    /// belongs to it, and is cleared before a pass of another type is projected.
    pub(crate) last_type: Mutex<Option<&'static str>>,
    /// How many trace rows the chat's store holds (#766, 1.10): RAM, read from the store
    /// once per process, before the chat's first trace append; then kept by the writer.
    pub(crate) trace_count: Mutex<Option<usize>>,
}

/// One message stored in the agent's memory and not yet read by a pass.
#[derive(Clone, Debug)]
pub(crate) struct Arrival {
    /// Its stored row's id.
    pub(crate) row_id: i64,
    /// The sanitised client id it came with.
    pub(crate) client_id: String,
    /// The sender's id (the calling agent, else the client id).
    pub(crate) origin: String,
    /// Its text, as stored.
    pub(crate) text: String,
    /// Its correlation id for status events.
    pub(crate) send_id: String,
    /// When it arrived.
    pub(crate) at: f64,
    /// This layer's own text (a notice).
    pub(crate) notice: bool,
}

/// THE AGENT'S LIFE (#795): it is woken by messages, one pass at a time.
#[derive(Default)]
pub(crate) struct Life {
    /// A pass is running: from its start to the write of its answer row.
    pub(crate) running: bool,
    /// The number of the pass in flight, raised when one starts and when `stop` or
    /// a cancel ends one: a request settle that names an older one is not acted on.
    pub(crate) epoch: u64,
    /// Messages stored and not yet read, in arrival order: the next pass reads them
    /// all.
    pub(crate) unread: Vec<Arrival>,
    /// The calls the agent's answers sent whose replies have not come yet.
    pub(crate) open: usize,
    /// Paused (#219): arrivals are stored and unread, and no pass starts.
    pub(crate) paused: bool,
    /// How many times a `stop`, a pause or a boot ended the agent's work: a reply its
    /// earlier answers routed, landing after, is recorded `late` and wakes nothing.
    pub(crate) stops: u64,
}

/// Builds the provider of a pass, its client id given.
pub(crate) type ProviderFor = Arc<dyn Fn(&str) -> Arc<dyn crate::provider::Provider> + Send + Sync>;

/// What a pass needs that only a call to the bundle hands over: the provider, and the
/// backend's knobs.
#[derive(Clone)]
pub(crate) struct Machine {
    /// Builds the provider of a pass (its client id given).
    pub(crate) provider: ProviderFor,
    /// The backend's knobs.
    pub(crate) cfg: crate::agent_loop::BackendConfig,
}

impl BackendState {
    /// The life changed: whoever waits for the agent to be quiet looks again.
    pub(crate) fn changed(&self) {
        self.life_changed.notify_waiters();
    }

    /// QUIET: no pass runs, nothing is unread, and no call its answers sent is still
    /// owed a reply.
    pub(crate) fn is_quiet(&self) -> bool {
        let life = self.life.lock().expect("life poisoned");
        !life.running && life.unread.is_empty() && life.open == 0
    }

    fn new() -> Arc<Self> {
        Arc::new(Self {
            current_meta: Mutex::new(None),
            life: Mutex::new(Life::default()),
            life_changed: tokio::sync::Notify::new(),
            machine: Mutex::new(None),
            projection: Mutex::new(None),
            compaction_mark: Mutex::new(None),
            folds_since: Mutex::new(0),
            recall_due: Mutex::new(false),
            view_source: Mutex::new(None),
            prev_request: Mutex::new(None),
            calib: Mutex::new(None),
            reported: Mutex::new(None),
            last_send_secs: Mutex::new(None),
            caps_checked: Mutex::new(None),
            recalled_images: Mutex::new(Vec::new()),
            line_summary: Mutex::new(None),
            model_seen: Mutex::new(None),
            model_key: Mutex::new(None),
            render_cache: Arc::new(crate::model_format::cache::RenderCache::new()),
            used_running: Mutex::new(None),
            used_pending: Mutex::new(crate::used::Pending::default()),
            last_type: Mutex::new(None),
            trace_count: Mutex::new(None),
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

/// `true` iff a pass is in flight right now. Read off `current_meta`.
pub fn is_generating(agent_id: &AgentId) -> bool {
    BACKENDS
        .lock()
        .get(agent_id)
        .map(|s| s.current_meta.lock().expect("current poisoned").is_some())
        .unwrap_or(false)
}

/// WAIT UNTIL `agent_id` IS QUIET ([`BackendState::is_quiet`]) or `within` passes:
/// `true` when it is. Woken by the life's changes, never a clock; the bound is only
/// the failure. Tests and harnesses read the end of an agent's work through it.
pub async fn until_quiet(agent_id: &AgentId, within: std::time::Duration) -> bool {
    let st = state_for(agent_id);
    let wait = async {
        loop {
            let changed = st.life_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if st.is_quiet() {
                return;
            }
            changed.await;
        }
    };
    tokio::time::timeout(within, wait).await.is_ok()
}

/// TEST SUPPORT: wait until EVERY agent of `kernel` is quiet — a reply one delivers may
/// wake another, so the agents are looked at again until one sweep finds them all quiet.
/// `false` when `within` passed first.
#[cfg(any(test, feature = "test-support"))]
pub async fn settle(kernel: &Arc<fantastic_kernel::Kernel>, within: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let mut all = true;
        for id in kernel.agent_ids() {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if !state_for(&id).is_quiet() {
                all = false;
                if !until_quiet(&id, left).await {
                    return false;
                }
            }
        }
        if all {
            return true;
        }
    }
}

/// Drop the agent's state slot (delete / shutdown). An in-flight `send` is a
/// KERNEL task — it dies with the agent's dispatches, not through a bundle-held
/// handle.
pub fn drop_state(agent_id: &AgentId) {
    let removed = BACKENDS.lock().remove(agent_id);
    // a pass whose request settles after this acts on nothing
    if let Some(state) = removed {
        let mut life = state.life.lock().expect("life poisoned");
        life.epoch += 1;
        life.running = false;
        life.unread.clear();
    }
}

/// Close out a pass the kernel is cancelling: if `task_id` names the request of
/// the pass in flight, clear the in-flight entry and hand back the
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

/// Take the in-flight entry of the pass whatever its request: a pass a cancel or a
/// stop closes.
pub(crate) fn take_current(agent_id: &AgentId) -> Option<CurrentEntry> {
    let state = BACKENDS.lock().get(agent_id).map(Arc::clone)?;
    let taken = state.current_meta.lock().expect("current poisoned").take();
    taken
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
pub fn status_snapshot(agent_id: &AgentId, client_id: Option<&str>) -> Value {
    // A CLIENT THAT NAMES ITSELF gets its own entries in full; an EMPTY or absent one
    // takes the redacted branch and the reply echoes what it sent.
    let requesting = client_id.filter(|c| !c.is_empty()).map(safe_client);
    let echoed_client = match (&requesting, client_id) {
        (Some(c), _) => json!(c),
        (None, Some(raw)) => json!(raw),
        (None, None) => Value::Null,
    };
    let state = state_for(agent_id);
    let cur = state.current_meta.lock().expect("current poisoned").clone();
    // THE MESSAGES NOT READ YET: stored, and read by the next pass.
    let unread: Vec<Arrival> = state.life.lock().expect("life poisoned").unread.clone();
    let pending_from: Vec<String> = unread.iter().map(|a| a.origin.clone()).collect();

    let current = cur
        .as_ref()
        .map(|c| current_out(c, requesting.as_deref()))
        .unwrap_or(Value::Null);

    let (mine_pending, others_pending) = match &requesting {
        Some(req) => {
            let mut mine = Vec::new();
            let mut others = 0u64;
            for a in &unread {
                if a.client_id == *req {
                    mine.push(json!({
                        "send_id": a.send_id,
                        "text": a.text,
                        "queued_at": a.at,
                    }));
                } else {
                    others += 1;
                }
            }
            (Value::Array(mine), others)
        }
        None => (Value::Array(Vec::new()), unread.len() as u64),
    };

    json!({
        "source": agent_id.as_str(),
        "client_id": echoed_client,
        "generating": cur.is_some(),
        "current": current,
        "mine_pending": mine_pending,
        "others_pending": others_pending,
        "pending_from": pending_from,
        "inquiry_from": crate::route::inquiry_askers(agent_id.as_str()),
    })
}
