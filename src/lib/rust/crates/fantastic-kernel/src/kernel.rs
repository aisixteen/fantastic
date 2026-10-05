//! Shared kernel context — flat agent index, inbox queues, bundle
//! registry, root pointer.
//!
//! The [`Kernel`] is `Arc`-cloneable and `Send + Sync` so handler
//! closures can capture it without contention. Concurrent paths use
//! lock-free `DashMap`s for the agent + inbox lookups; the root
//! pointer is an `ArcSwap` so swaps during boot are observable
//! atomically by every reader.

use crate::agent::{Agent, AgentId};
use crate::bundle::BundleRegistry;
use crate::errors::{KernelError, KernelResult};
use crate::state::{KernelState, CURRENT_VERSION};
use crate::storage::StorageMode;
use crate::tasks::TaskTable;
use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::Notify;

/// Bounded inbox capacity per agent — matches Python's `INBOX_BOUND`.
/// 256 fits a chat-paced workload comfortably and bounds memory on a
/// runaway emitter. Tunable via [`Kernel::new_with_inbox_bound`].
pub const DEFAULT_INBOX_BOUND: usize = 256;

/// A subscriber callback fired on every state event (`send` / `emit`
/// / `removed` / etc.). Synchronous, called inline — keep it cheap;
/// off-thread the work if it might block.
///
/// The second argument is the event payload's RAW BODY — its slot within
/// `payload` and the bytes ([`crate::outcome::Blob`]) — for a traffic event whose
/// payload carried bytes. Python
/// queues the bytes inside the payload dict and its WS tap re-encodes the whole
/// event as a binary frame; a `serde_json::Value` cannot hold bytes, so the body
/// rides beside the event exactly as it rides beside an
/// [`crate::inbox::InboxItem`]. `None` for every event with no bytes.
pub type StateSubscriber = Arc<dyn Fn(&Value, Option<&crate::outcome::Blob>) + Send + Sync>;

/// The registry of state taps. Agent lifecycle + traffic telemetry only:
/// conversation rows are NOT on it — the kernel holds none; an AI agent announces
/// its own rows to its watchers (`watch`, gated by that agent).
pub(crate) type StateSubs = RwLock<Vec<(SubscriberToken, StateSubscriber)>>;

/// [`Kernel::publish_state_with_body`]'s body, against the bare registry.
///
/// `ts` is stamped HERE and LAST (python `{**event, "ts": time.time()}`), so
/// every event carries the moment it was published and no caller has to
/// remember to add one. No subscribers = no work at all, as in python.
pub(crate) fn publish_state_to(
    subs: &StateSubs,
    event: &Value,
    body: Option<&crate::outcome::Blob>,
) {
    let subs = subs.read().expect("state subs poisoned");
    // Nothing inside the kernel's own ledger flush is published (audit CLI-B).
    if subs.is_empty() || crate::send::state_quiet() {
        return;
    }
    let mut stamped = match event {
        Value::Object(m) => m.clone(),
        other => {
            for (_token, cb) in subs.iter() {
                call_state_subscriber(cb, other, body);
            }
            return;
        }
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    stamped.insert("ts".to_string(), json!(ts));
    let stamped = Value::Object(stamped);
    for (_token, cb) in subs.iter() {
        call_state_subscriber(cb, &stamped, body);
    }
}

/// One subscriber call, ISOLATED (C87) — python: `try: cb(event) except
/// Exception as e: print(f"  [kernel] state subscriber raised: {e}",
/// file=sys.stderr)`, and the loop moves on to the next subscriber. A
/// rust callback has no `Result` to fail with — the equivalent of
/// "raised" is a panic, caught here so one broken tap never silences
/// every other subscriber for the rest of the process.
fn call_state_subscriber(cb: &StateSubscriber, event: &Value, body: Option<&crate::outcome::Blob>) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(event, body)));
    if let Err(payload) = outcome {
        eprintln!(
            "  [kernel] state subscriber raised: {}",
            crate::send::panic_message(&payload)
        );
    }
}

/// Opaque token returned by [`Kernel::add_state_subscriber`]; pass to
/// [`Kernel::remove_state_subscriber`] to detach.
///
/// The inner `u64` is `pub` so embedding consumers can round-trip the
/// value through a primitive type (e.g. a JSON string) across a
/// process boundary. Treat it as opaque from Rust callers — its
/// numeric value carries no meaning beyond "token issued by this
/// kernel".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberToken(pub u64);

/// Shared kernel context.
pub struct Kernel {
    /// Flat global id → Agent. The only routing table — a LOOKUP table, with
    /// no order of its own.
    pub agents: DashMap<AgentId, Arc<Agent>>,
    /// Registration order of [`Self::agents`], maintained by
    /// [`Self::register`] / [`Self::unregister`] and read through
    /// [`Self::agent_ids`].
    ///
    /// Python's registry is a plain dict, so `list_agents`, the shutdown sweep
    /// and the snapshot walk it in the order agents were registered. A
    /// `DashMap` has none, so the port sorted by id at each reader and answered
    /// a registry python never answers.
    agent_order: RwLock<Vec<AgentId>>,
    /// Per-id inbox (agents + synthetic browser client ids).
    pub inboxes: DashMap<AgentId, crate::inbox::InboxSender>,
    /// Active state-event taps (telemetry, debugging). Keyed by token
    /// for clean detach.
    state_subscribers: Arc<StateSubs>,
    /// Monotonic counter for [`SubscriberToken`].
    next_subscriber: AtomicU64,
    /// Compile-time bundle registry — caller fills before bootstrap.
    pub bundles: BundleRegistry,
    /// The root agent (`id="kernel_state"` by convention). Set during bootstrap.
    pub root: ArcSwapOption<Agent>,
    /// Per-inbox bound; same value applied to every channel.
    pub inbox_bound: usize,
    /// State medium. [`StorageMode::Disk`] auto-flushes to
    /// `<workdir>/.fantastic/state.json` on every mutation;
    /// [`StorageMode::InMemory`] never touches the filesystem. See
    /// [`Self::save`] / [`Self::load`] for the on-demand API both
    /// modes share.
    pub storage: StorageMode,
    /// Graceful-shutdown signal. The root-only `kernel_stop` verb
    /// calls [`Self::request_shutdown`] AFTER returning its ack; the CLI
    /// daemon awaits [`Self::shutdown_requested`] alongside SIGINT/SIGTERM
    /// and runs the SAME drain + lock-release + `exit 0` path. Lets an
    /// in-kernel verb stop the process remotely (over web_rest / web_ws),
    /// not just via an OS signal.
    shutdown: Notify,
    /// DRAINING. Set by `shutdown_agents`; a NEW ROOT send is refused with
    /// `draining` while it holds, so nothing fresh enters a tree that is
    /// stopping — a send with a live parent task still passes, because that is
    /// work already in flight finishing itself.
    draining: std::sync::atomic::AtomicBool,
    /// The ROOT LOADER's record writer, once its `boot` installed one. Held so
    /// a one-shot can drain it synchronously before the process exits, exactly
    /// as python's `call` mode awaits `root._fs_flush_loop.aflush()`.
    record_writer: std::sync::Mutex<Option<crate::persistence::PersistWriter>>,
    /// Has [`Self::shutdown`] already run? The SIGTERM handler and any atexit
    /// safety net must not double-walk the tree.
    shutdown_complete: std::sync::atomic::AtomicBool,
    /// The TASK TABLE — every in-flight dispatch, a tree through the send
    /// chain. The `ps` system verb reads it; `cancel` kills a task and its
    /// subtree. See [`crate::tasks`].
    pub tasks: TaskTable,

    /// THE EXECUTION LEDGER, process side — every execution still in RAM (in
    /// flight, or settled and not yet flushed), named (a caller's dedup contract)
    /// or unnamed (under its minted pid). Retention-ringed; the DURABLE half is
    /// the `call` + `settle` rows the kernel flushes into its memory store.
    pub ledger: crate::ledger::Ledger,
    /// THE EXECUTION ROWS — `call`, `settle`, `task`, `ack` rows the kernel minted
    /// and has NOT YET FLUSHED to its memory store, plus the ONE COUNTER every id
    /// comes from. The kernel keeps NO conversation.
    pub rows: crate::rows::RowStore,
    /// The kernel's memory store: which one (`kernel_start`), the flusher, the
    /// flush bookkeeping. See [`crate::executions`].
    pub(crate) exec: crate::executions::ExecState,
    /// Per-KERNEL state a bundle keeps ON the kernel's context, by key — python's
    /// `ctx._ai_memory` attribute: two kernels in one process (tests, a
    /// multi-kernel host) never share it. See [`Self::extension`].
    extensions: DashMap<&'static str, Arc<dyn std::any::Any + Send + Sync>>,
    /// CONNECTED PEER KERNELS — guid → the LIVE routes to it (owner, 2026-09-30;
    /// a set per guid, 2026-10-03: the tabs of one identity are one principal).
    /// An agent on another kernel is addressed `<guid>|<agent id>`; when that guid
    /// is here, `send` hands the call to its routes and the first reply is the
    /// call's ordinary outcome. A TRANSPORT registers the peer it is connected to
    /// and unregisters it when the connection ends. See [`crate::peers`].
    pub(crate) peers: crate::peers::PeerTable,
    /// The on-disk children-container dir name — `<agent>/<children_dir>/<child>`
    /// for EVERY agent of this tree. The root record declares it (a
    /// self-describing layout sets e.g. `host_agents`); [`Self::load`] reads it,
    /// default `agents`. Python `Kernel.children_dir`.
    children_dir: RwLock<String>,
}

/// The children-container dir name when the root record declares none.
pub const DEFAULT_CHILDREN_DIR: &str = "agents";

/// One bound for every agent's `shutdown` dispatch. Python
/// `Kernel.SHUTDOWN_VERB_TIMEOUT`.
const SHUTDOWN_VERB_TIMEOUT: f64 = 30.0;
/// How long in-flight work may take to settle before it is cancelled. Python
/// `Kernel.QUIET_TIMEOUT`.
const QUIET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a cancelled batch may take to record its ending. Python
/// `Kernel.SETTLE_TIMEOUT`.
const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    /// Empty kernel — no agents, no root, default inbox bound,
    /// [`StorageMode::InMemory`]. The `Disk` mode is opt-in via
    /// [`Self::with_storage`]; the no-arg ctor keeps the existing
    /// per-test default (these tests never persist).
    pub fn new() -> Self {
        Self::with_storage(StorageMode::InMemory)
    }

    /// Empty kernel with a specific [`StorageMode`]. Used by
    /// [`crate::bootstrap::bootstrap`] to pick `Disk` or `InMemory`
    /// based on caller options.
    pub fn with_storage(storage: StorageMode) -> Self {
        Self::with_storage_and_inbox(storage, DEFAULT_INBOX_BOUND)
    }

    /// Empty kernel with a custom inbox bound. Use for testing
    /// back-pressure paths.
    pub fn new_with_inbox_bound(bound: usize) -> Self {
        Self::with_storage_and_inbox(StorageMode::InMemory, bound)
    }

    /// Empty kernel with both a [`StorageMode`] and a custom inbox
    /// bound — the underlying ctor every other variant funnels into.
    pub fn with_storage_and_inbox(storage: StorageMode, bound: usize) -> Self {
        let state_subscribers: Arc<StateSubs> = Arc::new(RwLock::new(Vec::new()));
        Self {
            agents: DashMap::new(),
            agent_order: RwLock::new(Vec::new()),
            inboxes: DashMap::new(),
            state_subscribers,
            next_subscriber: AtomicU64::new(1),
            bundles: BundleRegistry::new(),
            root: ArcSwapOption::const_empty(),
            inbox_bound: bound,
            storage,
            shutdown: Notify::new(),
            draining: std::sync::atomic::AtomicBool::new(false),
            record_writer: std::sync::Mutex::new(None),
            shutdown_complete: std::sync::atomic::AtomicBool::new(false),
            tasks: TaskTable::new(),
            ledger: crate::ledger::Ledger::default(),
            rows: crate::rows::RowStore::new(),
            exec: crate::executions::ExecState::default(),
            extensions: DashMap::new(),
            peers: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            children_dir: RwLock::new(DEFAULT_CHILDREN_DIR.to_string()),
        }
    }

    /// The children-container dir name of this tree (python `ctx.children_dir`).
    #[must_use]
    pub fn children_dir_name(&self) -> String {
        self.children_dir
            .read()
            .expect("children_dir poisoned")
            .clone()
    }

    /// Where `agent`'s children live — python `Agent._children_dir()`:
    /// `<root_path>/<ctx.children_dir>`.
    #[must_use]
    pub fn children_dir_of(&self, agent: &Agent) -> PathBuf {
        agent.root_path.join(self.children_dir_name())
    }

    /// The bundle state kept ON this kernel under `key`, created on first use.
    /// One kernel's value is never another kernel's, even in one process.
    ///
    /// # Panics
    /// When `key` was first created with a different type — a programming error.
    pub fn extension<T: Default + Send + Sync + 'static>(&self, key: &'static str) -> Arc<T> {
        let any = self
            .extensions
            .entry(key)
            .or_insert_with(|| Arc::new(T::default()) as Arc<dyn std::any::Any + Send + Sync>)
            .clone();
        any.downcast::<T>()
            .unwrap_or_else(|_| panic!("kernel extension {key:?} holds another type"))
    }

    /// Patch an agent's meta — python `Agent.update(agent_id, **meta)`: the meta
    /// is merged, and the `updated` state event (the CHANGED KEYS) is what drives
    /// a loader to re-persist the record. No disk write here. `None` when no such
    /// agent.
    pub fn update(
        &self,
        id: &AgentId,
        patch: serde_json::Map<String, Value>,
    ) -> Option<crate::agent::AgentRecord> {
        let agent = self.agents.get(id).map(|e| Arc::clone(&e))?;
        let changed: Vec<Value> = patch.keys().map(|k| Value::String(k.clone())).collect();
        let rec = agent.update_meta(patch);
        let mut ev = serde_json::Map::new();
        ev.insert("agent_id".to_string(), Value::String(id.0.clone()));
        ev.insert("kind".to_string(), Value::String("updated".to_string()));
        ev.insert("changed".to_string(), Value::Array(changed));
        self.publish_state(&Value::Object(ev));
        Some(rec)
    }

    /// THE COUNTER COMES BACK WITH THE GUID: the root record's reserved bound
    /// (`next_id`) is where this run starts, and this run's first block is
    /// reserved in memory. Never edits a record (save → load → save is
    /// identical); `kernel_start` writes the bound, and a started kernel persists
    /// every later block. Python `Kernel.load`'s tail.
    pub fn restore_counter(&self, root_meta: &serde_json::Map<String, Value>) {
        let nid = root_meta
            .get("next_id")
            .filter(|v| !v.is_boolean())
            .and_then(Value::as_i64);
        self.rows.restore_counter(nid);
    }

    /// Signal the daemon serve loop to begin a graceful shutdown. Called
    /// by the root-only `kernel_stop` verb AFTER it has produced its
    /// ack reply, so the reply flushes over the web surface before the
    /// process exits. Idempotent — extra calls collapse to one shutdown
    /// (the serve loop awaits this once, then unwinds straight-line).
    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Resolves once [`Self::request_shutdown`] has been called. The CLI
    /// daemon awaits this in its `tokio::select!` next to SIGINT/SIGTERM;
    /// when it fires, the daemon drains agents, releases `lock.yaml`, and
    /// returns `ExitCode::SUCCESS`.
    pub async fn shutdown_requested(&self) {
        self.shutdown.notified().await;
    }

    /// Is the kernel draining? New ROOT sends are refused while it is.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.draining.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Remember the record writer the root loader's `boot` installed.
    pub fn set_record_writer(&self, w: crate::persistence::PersistWriter) {
        *self.record_writer.lock().expect("writer poisoned") = Some(w);
    }

    /// Forget it (the writer stopped).
    pub fn clear_record_writer(&self) {
        *self.record_writer.lock().expect("writer poisoned") = None;
    }

    /// Queue a record for the root loader's writer with NO state event — the
    /// id-block reservation inside the silent ledger flush (audit CLI-B). A no-op
    /// when no loader has booted.
    pub(crate) fn mark_record_dirty(&self, id: &AgentId) {
        let w = self.record_writer.lock().expect("writer poisoned").clone();
        if let Some(w) = w {
            w.mark_dirty(self, id);
        }
    }

    /// Drain the record writer's dirty set NOW — python's `aflush`. A no-op
    /// when no loader has booted (nothing is queued and nothing would land).
    pub async fn flush_records(self: &Arc<Self>) {
        let w = self.record_writer.lock().expect("writer poisoned").clone();
        if let Some(w) = w {
            w.flush(self).await;
        }
    }

    /// GRACEFUL TREE-WIDE PROCESS SHUTDOWN — python `Kernel.shutdown`.
    ///
    /// [`Self::shutdown_agents`] (drain, the `shutdown` verb, quiet), then the
    /// ROOT bundle's `flush_pending` — awaited BEFORE the teardown walk, so the
    /// persister can write its last window while its provider child is still
    /// alive — then the DEPTH-FIRST `on_shutdown` walk that frees OS resources
    /// (PTYs, subprocesses, sockets) without touching records or disk. The tree
    /// survives, so the next boot rehydrates cleanly.
    ///
    /// IDEMPOTENT: a second call does nothing. Nothing used to call
    /// `Bundle::on_shutdown` at all — the daemon sent `{type:shutdown}` LIFO to
    /// every agent, which most bundles answer "unknown type".
    pub async fn shutdown(self: &Arc<Self>) {
        if self
            .shutdown_complete
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let Some(root) = self.root() else { return };
        self.shutdown_agents().await;
        if let Some(bundle) = root
            .handler_module
            .as_deref()
            .and_then(|hm| self.bundles.get(hm))
        {
            // Its writes are the kernel's own traffic (python wraps the hook in
            // `shutdown_context()`): written while draining, the store would
            // answer "draining" and the last records would never land.
            if let Err(e) = crate::send::with_shutdown(bundle.flush_pending(&root.id, self)).await {
                eprintln!("  [shutdown] flush_pending raised: {e}");
            }
        }
        if self.started() {
            // THE LAST EXECUTION ROWS, while the store is still alive: everything
            // settled by the drain (done, or cancelled into the dead letters)
            // lands.
            let _ = crate::send::with_shutdown(self.flush_executions()).await;
            self.stop_flusher().await;
        }
        self.shutdown_walk(&root).await;
    }

    /// Does this agent's bundle DECLARE `verb` — python's
    /// `Agent.advertises(verb)` (`_agent.py:1160`), which reads the module's
    /// `VERBS` table and dispatches nothing.
    ///
    /// Rust keeps its verb table inside the bundle's own reflect reply, so the
    /// read calls `Bundle::handle({"type":"reflect"})` DIRECTLY — never through
    /// `send`. A dispatch would mint a message id and write a call row, a task
    /// row, a ledger execution and a watcher echo for a question python answers
    /// with an attribute lookup.
    ///
    /// False for a bare agent, for a handler_module no registry holds (python's
    /// unimportable bundle advertises nothing) and for a bundle that raised.
    pub(crate) async fn advertises(self: &Arc<Self>, id: &AgentId, verb: &str) -> bool {
        let Some(agent) = self.agents.get(id).map(|e| Arc::clone(&e)) else {
            return false;
        };
        let Some(bundle) = agent
            .handler_module
            .as_deref()
            .and_then(|hm| self.bundles.get(hm))
        else {
            return false;
        };
        let reply = bundle
            .handle(id, &json!({"type": "reflect"}), self)
            .await
            .unwrap_or(None)
            .unwrap_or(Value::Null);
        reply
            .get("verbs")
            .and_then(Value::as_object)
            .is_some_and(|v| v.contains_key(verb))
    }

    /// DEPTH-FIRST `on_shutdown`, children before their parent. A hook that
    /// errors is printed and the walk continues — teardown is not refusable.
    fn shutdown_walk<'a>(
        self: &'a Arc<Self>,
        agent: &'a Arc<Agent>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            for cid in agent.child_ids() {
                if let Some(child) = self.agents.get(&cid).map(|e| Arc::clone(&e)) {
                    self.shutdown_walk(&child).await;
                }
            }
            let Some(hm) = agent.handler_module.as_deref() else {
                return;
            };
            let Some(bundle) = self.bundles.get(hm) else {
                // python imports the module to find its hook and SAYS when it
                // cannot (`print(f"  [shutdown] {self.id} import {hm!r} raised:
                // {e}", file=sys.stderr)`) — an agent whose bundle this runtime
                // lacks is named in the log, not skipped silently.
                eprintln!(
                    "  [shutdown] {} import {} raised: {}",
                    agent.id.0,
                    fantastic_bundle::py_repr(hm),
                    crate::send::no_module_named(hm)
                );
                return;
            };
            // C87: python's exact text is "hook raised", not "on_shutdown
            // raised" (`print(f"  [shutdown] {self.id} hook raised: {e}",
            // file=sys.stderr)`).
            if let Err(e) = bundle.on_shutdown(&agent.id, self).await {
                eprintln!("  [shutdown] {} hook raised: {e}", agent.id.0);
            }
        })
    }

    /// THE AGENT HALF OF A GRACEFUL STOP — also the `shutdown_agents` kernel
    /// verb.
    ///
    /// 1. DRAIN: new roots are refused; sends with a live parent task still pass.
    /// 2. `shutdown` VERB: every live agent is told, all at once, bounded. For
    ///    an AI it is a message ("the kernel is stopping; persist what matters");
    ///    its tools work, because the kernel keeps serving.
    /// 3. QUIET: wait for the task table to empty (keepalive loops excepted),
    ///    bounded; the rest is cancelled through the normal `cancel_task` path,
    ///    so `on_cancel` hooks run and subtrees die with their roots.
    ///
    /// Idempotent: a second call answers the same shape and shuts nothing new.
    /// The kernel keeps serving afterwards — reads, `ledger`, the flusher —
    /// until the process exits.
    ///
    /// WHO GETS TOLD is python's `advertises("shutdown")`: does this agent's
    /// bundle DECLARE the verb. Python reads the module's `VERBS` table — a
    /// pure attribute read that dispatches nothing. Rust's declaration lives in
    /// the bundle's own reflect map, so the read is [`Self::advertises`]: the
    /// handler is called DIRECTLY, with no message id, no call row, no task
    /// table entry, no ledger execution and no watcher fanout.
    ///
    /// It used to go through `send`, and that wrote one `call`/`reflect` row per
    /// agent per shutdown — plus, through `apply_reflect_flags`, an `internal`
    /// roster row on every node of the tree — where python writes nothing at
    /// all. Measured against the python baseline: 3 extra rows per agent on
    /// every reboot (`e2e/parity` `ctx/reboot/*_rows_read.json`).
    pub async fn shutdown_agents(self: &Arc<Self>) -> Value {
        self.draining
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Not every agent: a bundle that merely ACCEPTS the verb as a lifecycle
        // no-op is not asking to be told, and a face that treats it as a
        // teardown must never be told by the stop it is carrying (the web
        // listener, 2026-09-12).
        let all: Vec<AgentId> = self.agent_ids();
        let mut targets: Vec<AgentId> = Vec::new();
        for id in &all {
            if self.advertises(id, "shutdown").await {
                targets.push(id.clone());
            }
        }
        let mut shut: Vec<String> = Vec::new();
        if !targets.is_empty() {
            // KERNEL BOOKKEEPING: each agent's handler is run directly
            // ([`Self::bookkeeping`]) — no gate, so every advertising agent is told
            // at any depth, whoever's dispatch carries the stop — all at once,
            // each bounded, and EXEMPT FROM THE DRAIN GATE: this IS the shutdown.
            let tell = |id: &AgentId| {
                let agent = self.agents.get(id).map(|e| Arc::clone(&e));
                async move {
                    let Some(agent) = agent else {
                        return crate::outcome::Status::Failed;
                    };
                    let told = self.bookkeeping(&agent, json!({"type": "shutdown"}), None);
                    match tokio::time::timeout(
                        std::time::Duration::from_secs_f64(SHUTDOWN_VERB_TIMEOUT),
                        told,
                    )
                    .await
                    {
                        Ok(o) => o.status,
                        Err(_) => crate::outcome::Status::TimedOut,
                    }
                }
            };
            let outs = crate::send::with_shutdown(futures_util::future::join_all(
                targets.iter().map(tell),
            ))
            .await;
            for (id, status) in targets.iter().zip(outs) {
                if status == crate::outcome::Status::Completed {
                    shut.push(id.0.clone());
                }
            }
        }
        // The dispatch running THIS verb is in the table too; it is not
        // pending work — nor is the chain carrying it (over a wire, the leg's
        // `inbound_call` is its root: cancelling it cancels this verb's own reply).
        let mine = self.tasks.carriers(crate::tasks::current_task());
        let deadline = std::time::Instant::now() + QUIET_TIMEOUT;
        // QUIET WAITS ON DISPATCHES ONLY — a forked service loop never settles.
        while std::time::Instant::now() < deadline
            && !self.tasks.pending_dispatches(&mine).is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let mut cancelled: Vec<Value> = Vec::new();
        for tid in self.tasks.pending(&mine) {
            // ROOTS ONLY: the subtree dies with its root, so cancelling a child
            // here would race the parent's own teardown.
            if !self.tasks.is_root(&tid) {
                continue;
            }
            let out = self.cancel_task(&tid).await;
            if let Some(killed) = out.get("killed").and_then(Value::as_array) {
                cancelled.extend(killed.iter().cloned());
            }
        }
        // EVERY CALL SETTLES before this returns. A killed dispatch's batch records
        // its `cancelled` outcome (and its settle row) a few turns after the abort;
        // the stop that follows flushes the last rows, so it must not start before
        // those endings exist. Bounded: a batch that never settles is not waited
        // on forever.
        let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
        while std::time::Instant::now() < deadline
            && self
                .ledger
                .unsettled()
                .iter()
                .any(|eid| !self.tasks.contains(&crate::tasks::TaskId(*eid)))
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        json!({"drained": true, "shut": shut, "cancelled": cancelled})
    }

    /// Register an agent in the flat index. NO INBOX.
    ///
    /// An agent does not get a queue merely by existing. `register` used to mint
    /// one and hand back the receiver, which almost every caller dropped on the
    /// same line — so the queue was live, nobody read it, and `watch` happily
    /// mirrored into it forever. [`Self::ensure_inbox`] IS the reader
    /// registration, and every caller of it drains what it gets.
    pub fn register(&self, agent: Arc<Agent>) {
        // Re-registering an id keeps its place, as python's `dict[k] = v` does.
        {
            let mut order = self.agent_order.write().expect("agent order poisoned");
            if !order.contains(&agent.id) {
                order.push(agent.id.clone());
            }
        }
        self.agents.insert(agent.id.clone(), agent);
    }

    /// Resolve a `send`-style operand end to end: `"kernel"` names the root;
    /// otherwise a literal id. `None` when it names no live agent. The ONE place
    /// `send`'s target (text or binary item), `create_agent`'s `parent_id` and
    /// the gate's operand resolution all go through, so they cannot drift on
    /// what "the same id" resolves to.
    #[must_use]
    pub fn resolve_target(&self, raw_id: &str) -> Option<Arc<Agent>> {
        if raw_id == "kernel" {
            return self.root();
        }
        self.agents
            .get(&AgentId::from(raw_id))
            .map(|e| Arc::clone(&e))
    }

    /// Register `id` as a READER and hand back its queue — python's
    /// `Kernel.ensure_inbox`.
    ///
    /// This call IS the registration: a queue exists iff someone called this to
    /// get one, and every caller that does drains it (the WS proxy for a
    /// synthetic browser client, exec_bridge for a relay proxy, a test asserting
    /// on traffic). Idempotent, so a reader may call it defensively before every
    /// read — a second call reads the SAME queue, never a fresh one.
    pub fn ensure_inbox(&self, id: &AgentId) -> crate::inbox::InboxReceiver {
        use dashmap::mapref::entry::Entry;
        match self.inboxes.entry(id.clone()) {
            Entry::Occupied(e) => e.get().receiver(),
            Entry::Vacant(e) => {
                let (tx, rx) = crate::inbox::inbox(self.inbox_bound);
                e.insert(tx);
                rx
            }
        }
    }

    /// Drop `id`'s inbox — the reader is gone (a browser disconnected, a relay
    /// proxy was released). Idempotent.
    pub fn remove_inbox(&self, id: &AgentId) {
        self.inboxes.remove(id);
    }

    /// Every registered id, in REGISTRATION ORDER — python's registry walk.
    ///
    /// The one ordered view of [`Self::agents`]. Readers that answer a caller
    /// (`list_agents`, the shutdown sweep, the snapshot) go through here; a
    /// reader that only needs membership or a lookup uses `agents` directly.
    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agent_order
            .read()
            .expect("agent order poisoned")
            .clone()
    }

    /// Unregister an agent + drop its inbox. Caller has already run
    /// the bundle's `on_delete` hook + rmtreed the dir.
    pub fn unregister(&self, id: &AgentId) {
        self.inboxes.remove(id);
        self.agents.remove(id);
        self.agent_order
            .write()
            .expect("agent order poisoned")
            .retain(|a| a != id);
    }

    /// Cheap clone of the current root pointer (`None` until
    /// bootstrap completes).
    pub fn root(&self) -> Option<Arc<Agent>> {
        self.root.load_full()
    }

    /// Swap the root pointer atomically. Bootstrap calls this once.
    pub fn set_root(&self, root: Arc<Agent>) {
        self.root.store(Some(root));
    }

    /// The kernel's RUNNING DIR — the dir holding its root's `.fantastic`
    /// (python `Kernel.workdir`). The ONE source of the disk clamp base; a
    /// relative root address is resolved the way any relative path is.
    ///
    /// # Errors
    /// No root ⇒ no running dir to clamp disk to.
    pub fn workdir(&self) -> Result<PathBuf, String> {
        let root = self
            .root()
            .ok_or_else(|| "kernel has no root — no running dir to clamp disk to".to_string())?;
        if root.root_path.as_os_str().is_empty() {
            return Err(
                "the kernel root has no disk address — no running dir to clamp disk to".to_string(),
            );
        }
        let at = if root.root_path.is_absolute() {
            root.root_path.clone()
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(&root.root_path)
        };
        at.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("root address {} has no parent dir", at.display()))
    }

    /// Push a state event to every subscriber. Keep callbacks short —
    /// they run on the dispatching task.
    ///
    /// `ts` is stamped HERE and LAST (python `{**event, "ts": time.time()}`), so
    /// every event carries the moment it was published and no caller has to
    /// remember to add one. No subscribers = no work at all, as in python.
    pub fn publish_state(&self, event: &Value) {
        self.publish_state_with_body(event, None);
    }

    /// [`Self::publish_state`] for a traffic event whose payload carried bytes:
    /// the raw body travels beside the event so a tap can rebuild the binary
    /// frame python sends.
    pub fn publish_state_with_body(&self, event: &Value, body: Option<&crate::outcome::Blob>) {
        publish_state_to(&self.state_subscribers, event, body);
    }

    /// Register a state-event tap. Returns a token the caller can
    /// pass to [`Self::remove_state_subscriber`] to detach.
    pub fn add_state_subscriber(&self, cb: StateSubscriber) -> SubscriberToken {
        let token = SubscriberToken(self.next_subscriber.fetch_add(1, Ordering::SeqCst));
        self.state_subscribers
            .write()
            .expect("state subs poisoned")
            .push((token, cb));
        token
    }

    /// Detach a previously-registered subscriber. No-op if `token`
    /// isn't (or no longer is) registered.
    pub fn remove_state_subscriber(&self, token: SubscriberToken) {
        self.state_subscribers
            .write()
            .expect("state subs poisoned")
            .retain(|(t, _)| *t != token);
    }

    // ── save / load — the foundation primitive both storage modes share ──

    /// Snapshot the live kernel state as a [`KernelState`] value.
    /// Both [`StorageMode::Disk`] and [`StorageMode::InMemory`]
    /// kernels produce equal output for equal in-memory state — the
    /// medium difference is irrelevant here.
    ///
    /// Pure read; the records are SORTED BY ID (python `save`'s
    /// `records.sort(key=id)`), so [`Self::save_json`] is byte-deterministic for
    /// equal kernel state. Ephemeral agents are skipped (they're per-process
    /// composition, never round-trip through a snapshot).
    pub fn save(&self) -> KernelState {
        // TRANSITIVELY. Dropping only the ephemeral agent ITSELF left its
        // children behind as orphans — records whose `parent_id` names an agent
        // that is not in the document — and `load()` rejects exactly that. So
        // `save()` emitted a snapshot its own loader refused.
        let ephemeral: std::collections::HashSet<AgentId> = self
            .agents
            .iter()
            .filter(|e| e.value().ephemeral)
            .map(|e| e.key().clone())
            .collect();
        let dropped = {
            let mut dropped = ephemeral.clone();
            // Walk down until nothing new is reached: a child of a dropped agent
            // is dropped, and so is ITS child.
            loop {
                let mut grew = false;
                for e in self.agents.iter() {
                    let a = e.value();
                    if dropped.contains(e.key()) {
                        continue;
                    }
                    let orphaned = a.parent_id.as_ref().is_some_and(|p| dropped.contains(p));
                    if orphaned {
                        dropped.insert(e.key().clone());
                        grew = true;
                    }
                }
                if !grew {
                    break dropped;
                }
            }
        };
        let mut agents: Vec<_> = self
            .agent_ids()
            .into_iter()
            .filter(|id| !dropped.contains(id))
            .filter_map(|id| self.agents.get(&id).map(|e| e.value().record()))
            .collect();
        // python `records.sort(key=lambda r: r["id"])`.
        agents.sort_by(|a, b| a.id.cmp(&b.id));
        KernelState {
            version: CURRENT_VERSION,
            agents,
        }
    }

    /// [`Self::save`] serialized to JSON. Used by [`Self::maybe_flush`]
    /// (Disk auto-sync) and by the embedding API.
    /// Output is byte-deterministic for equal kernel states.
    pub fn save_json(&self) -> String {
        serde_json::to_string(&self.save()).expect("KernelState is JSON-serializable")
    }

    /// Replace this kernel's agent tree with `state`. Drops every
    /// currently-registered agent (including their inboxes), then
    /// rebuilds from the snapshot.
    ///
    /// Does NOT fire `boot` hooks — matches the existing
    /// bootstrap's own semantics. The bootstrap caller
    /// dispatches `{type:"boot"}` to whichever agents need it
    /// (typically the web agent, or a bundle that boots paired agents,
    /// etc.). For brain-kernel-style snapshot reload, the consumer
    /// can iterate `kernel.agents` post-load and fire boot themselves
    /// if needed.
    ///
    /// Weak-load: agents whose `handler_module` isn't in this
    /// kernel's [`BundleRegistry`] are logged + skipped along with
    /// their entire subtree (a parent's missing bundle implies all
    /// its descendants are unreachable). The record is dropped from
    /// the loaded set; subsequent [`Self::save`] won't see it.
    ///
    /// Errors:
    /// - [`KernelError::InvalidSnapshot`] if `state.version` exceeds
    ///   [`CURRENT_VERSION`], if the snapshot has no root, or if any
    ///   record's `parent_id` doesn't point at a record in the
    ///   snapshot.
    pub fn load(&self, state: KernelState) -> KernelResult<()> {
        if state.version > CURRENT_VERSION {
            return Err(KernelError::InvalidSnapshot(format!(
                "snapshot version {} exceeds this kernel's max ({})",
                state.version, CURRENT_VERSION
            )));
        }
        let records: Vec<Map<String, Value>> = state
            .agents
            .iter()
            .filter_map(|r| crate::jsonsafe::record_value(r).as_object().cloned())
            .collect();
        self.load_records(records)
    }

    /// [`Self::load`] for the RAW records a document holds — python
    /// `Kernel.load(records)`: [`validate_records`] FIRST (a bad document is a
    /// no-op, the live tree untouched), then the children-dir name off the root
    /// record, then the tree rebuilt.
    pub fn load_records(&self, records: Vec<Map<String, Value>>) -> KernelResult<()> {
        validate_records(&records)?;
        let mut agents: Vec<crate::agent::AgentRecord> = Vec::with_capacity(records.len());
        for r in records {
            let rec: crate::agent::AgentRecord = serde_json::from_value(Value::Object(r))
                .map_err(|e| KernelError::InvalidSnapshot(e.to_string()))?;
            agents.push(rec);
        }
        let state = KernelState {
            version: CURRENT_VERSION,
            agents,
        };
        // The root record declares this tree's children-dir name (default
        // "agents") — set BEFORE the build, since every agent derives its
        // address from it.
        if let Some(root) = state.agents.iter().find(|r| r.parent_id.is_none()) {
            *self.children_dir.write().expect("children_dir poisoned") = root
                .meta
                .get("children_dir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_CHILDREN_DIR)
                .to_string();
        }

        // Drop the current tree. Inboxes close as their senders are
        // dropped from the DashMap (subscribers see the channel
        // closing and clean up).
        self.agents.clear();
        self.agent_order
            .write()
            .expect("agent order poisoned")
            .clear();
        self.inboxes.clear();
        self.root.store(None);

        // Compute the per-agent on-disk root_path. Disk mode roots at
        // `<workdir>/.fantastic/`; children nest under `agents/<id>/`.
        // InMemory mode uses an empty PathBuf (never read).
        let disk_root: Option<PathBuf> = self.storage.workdir().map(|w| w.join(".fantastic"));

        // Two-pass: build all Agent structs first (parent_id strings
        // only), then wire parent/child Arc references. The
        // weak-load skip happens in the first pass — a record whose
        // handler_module isn't registered is dropped, along with any
        // descendant whose ancestor chain hits the dropped record.
        let mut by_id: std::collections::HashMap<String, Arc<Agent>> =
            std::collections::HashMap::new();
        let mut skip_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Topologically order: roots first, then BFS by ancestor depth.
        // Simpler approach: iterate; if a record's parent_id is a
        // skipped id, skip this one too. Repeat until no progress.
        // For sane snapshots (≤ a few thousand records) the cost is
        // O(N * depth), negligible.
        let mut pending: Vec<&crate::agent::AgentRecord> = state.agents.iter().collect();
        // Sort so parents always precede children when parent_id is
        // None or refers to an earlier record. Simple stable sort by
        // (parent_depth, id) — implemented by a multi-pass walk.
        let mut made_progress = true;
        while made_progress && !pending.is_empty() {
            made_progress = false;
            let mut next: Vec<&crate::agent::AgentRecord> = Vec::new();
            for rec in pending.drain(..) {
                let parent_id = rec.parent_id.clone();
                if let Some(pid) = parent_id.as_ref() {
                    if skip_ids.contains(pid) {
                        // Cascade skip — parent was dropped (weak-load).
                        // SILENT, as python is: `_kernel.py:1019-1031` returns
                        // without a line for the skipped subtree (A15.3).
                        skip_ids.insert(rec.id.clone());
                        made_progress = true;
                        continue;
                    }
                    if !by_id.contains_key(pid) {
                        // Parent not yet processed — re-queue.
                        next.push(rec);
                        continue;
                    }
                }

                // Weak-load: refuse this record if its handler_module
                // isn't registered. Root has no handler_module so
                // always loads.
                if let Some(hm) = rec.handler_module.as_deref() {
                    if self.bundles.get(hm).is_none() {
                        eprintln!(
                            "[kernel] skipping agent {}: bundle {} not installed in this runtime",
                            rec.id, hm,
                        );
                        skip_ids.insert(rec.id.clone());
                        made_progress = true;
                        continue;
                    }
                }

                // Construct the root_path. Disk mode: agent path is
                // `<workdir>/.fantastic[/agents/<id>]*` walking up the
                // parent chain. InMemory mode: empty PathBuf.
                let root_path = match (&disk_root, rec.parent_id.as_ref()) {
                    // Disk root.
                    (Some(disk), None) => disk.clone(),
                    // Disk child — reuse parent's already-composed
                    // root_path so we get `<disk>/agents/<id1>/agents/<id2>/...`.
                    (Some(_), Some(pid)) => self
                        .children_dir_of(by_id.get(pid).expect("parent processed before child"))
                        .join(&rec.id),
                    // InMemory — root_path is never read; empty sentinel.
                    (None, _) => PathBuf::new(),
                };

                let agent = Agent::new(
                    AgentId::from(rec.id.as_str()),
                    rec.handler_module.clone(),
                    rec.parent_id.as_ref().map(|s| AgentId::from(s.as_str())),
                    rec.meta.clone(),
                    root_path,
                    false, // not ephemeral — ephemeral agents are never in a snapshot
                );
                self.register(Arc::clone(&agent));
                by_id.insert(rec.id.clone(), Arc::clone(&agent));

                // Wire parent → child Arc.
                if let Some(pid) = &rec.parent_id {
                    if let Some(parent_arc) = by_id.get(pid) {
                        parent_arc.add_child(Arc::clone(&agent));
                    }
                } else {
                    self.set_root(Arc::clone(&agent));
                }

                made_progress = true;
            }
            pending = next;
        }

        if !pending.is_empty() {
            // Cycle or orphan whose parent never appeared (shouldn't
            // happen — validation above caught this).
            return Err(KernelError::InvalidSnapshot(format!(
                "could not resolve {} agents during load (cycle?)",
                pending.len()
            )));
        }
        if let Some(root) = state.agents.iter().find(|r| r.parent_id.is_none()) {
            self.restore_counter(&root.meta);
        }

        Ok(())
    }

    /// JSON-string form of [`Self::load`]. Parses then loads. Used by
    /// the embedding API and the Disk-mode boot path.
    pub fn load_json(&self, json: &str) -> KernelResult<()> {
        let state: KernelState = serde_json::from_str(json)
            .map_err(|e| KernelError::InvalidSnapshot(format!("parse: {e}")))?;
        self.load(state)
    }

    /// Synchronous read of every loaded agent's identity + in-flight handler
    /// count. Mirrors python's `Kernel.state_snapshot` — used by new
    /// `state_subscribe` subscribers to bootstrap their agent view before the
    /// first event arrives. No queue puts, no fanout.
    ///
    /// Each entry: `{agent_id, name, backlog}`. REGISTRATION ORDER (python walks
    /// `self.agents.values()`); it used to sort by id and report `backlog: 0` for
    /// everyone, which answered a registry and a workload python never answers.
    pub fn state_snapshot(&self) -> Vec<Value> {
        self.agent_ids()
            .into_iter()
            .filter_map(|id| self.agents.get(&id).map(|e| Arc::clone(&e)))
            .map(|a| {
                let name = a.name_or_id();
                json!({
                    "agent_id": a.id.0,
                    "name": name,
                    "backlog": a.in_flight(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;

/// Validate a flat record list before anything is torn down; `Ok` names the one
/// root. Python `kernel/_state.py` `validate_records`, check for check and text
/// for text: a falsy id, the id rule a create applies (`guard_agent_id`), the
/// reserved keys (sorted), a duplicate, not exactly one root, a parent outside
/// the set, and records unreachable from the root.
///
/// # Errors
/// [`KernelError::InvalidSnapshot`] carrying python's `SnapshotError` text.
pub fn validate_records(records: &[Map<String, Value>]) -> KernelResult<String> {
    use fantastic_bundle::{py_repr, py_repr_value, py_truthy};
    let bad = |m: String| Err(KernelError::InvalidSnapshot(m));
    let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut roots: Vec<String> = Vec::new();
    for r in records {
        let Some(rid) = r.get("id").filter(|v| py_truthy(v)) else {
            return bad("snapshot record missing id".to_string());
        };
        // THE SAME ID RULE AS A CREATE: a record edited on disk to carry the
        // reserved `kernel`, a foreign `guid|agent`, or a path-walking id is
        // refused here, before the live tree is torn down.
        if let Some(refused) = crate::refusal::guard_agent_id("load", rid.as_str().unwrap_or("")) {
            return bad(format!(
                "record {}: {}",
                py_repr_value(rid),
                refused
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ));
        }
        let rid = rid
            .as_str()
            .expect("guard_agent_id passed a str")
            .to_string();
        // RESERVED KEYS: constructor parameters, not meta.
        let mut reserved: Vec<&str> = crate::agent::RESERVED_RECORD_KEYS
            .iter()
            .copied()
            .filter(|k| r.contains_key(*k))
            .collect();
        if !reserved.is_empty() {
            reserved.sort_unstable();
            let listed: Vec<String> = reserved.iter().map(|k| py_repr(k)).collect();
            return bad(format!(
                "record {} carries reserved key(s) [{}] — these are Agent constructor \
                 parameters, not meta; the tree lives where its document lives and is \
                 never named in a record",
                py_repr(&rid),
                listed.join(", ")
            ));
        }
        if !ids.insert(rid.clone()) {
            return bad(format!("duplicate agent id {} in snapshot", py_repr(&rid)));
        }
        if r.get("parent_id").is_none_or(Value::is_null) {
            roots.push(rid);
        }
    }
    if roots.is_empty() {
        return bad("snapshot has no root (no record with parent_id == null)".to_string());
    }
    if roots.len() > 1 {
        return bad(format!(
            "snapshot has {} roots; expected exactly one",
            roots.len()
        ));
    }
    for r in records {
        if let Some(pid) = r.get("parent_id").filter(|p| !p.is_null()) {
            if !pid.as_str().is_some_and(|p| ids.contains(p)) {
                return bad(format!(
                    "agent {} parent {} not in snapshot",
                    py_repr_value(r.get("id").unwrap_or(&Value::Null)),
                    py_repr_value(pid)
                ));
            }
        }
    }
    // REACHABILITY. A CYCLE (a -> b -> a) resolves on both ends and is
    // unreachable from the root; caught here, with the other pre-teardown checks.
    let mut kids: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for r in records {
        if let Some(p) = r.get("parent_id").and_then(Value::as_str) {
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
            kids.entry(p.to_string()).or_default().push(id.to_string());
        }
    }
    let mut reached: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack = vec![roots[0].clone()];
    while let Some(node) = stack.pop() {
        if !reached.insert(node.clone()) {
            continue;
        }
        if let Some(cs) = kids.get(&node) {
            stack.extend(cs.iter().cloned());
        }
    }
    let mut orphans: Vec<&String> = ids.iter().filter(|id| !reached.contains(*id)).collect();
    if !orphans.is_empty() {
        orphans.sort_unstable();
        let listed: Vec<String> = orphans.iter().map(|o| py_repr(o)).collect();
        return bad(format!(
            "snapshot has {} record(s) unreachable from the root {} (a cycle or an \
             orphaned parent chain): [{}]",
            orphans.len(),
            py_repr(&roots[0]),
            listed.join(", ")
        ));
    }
    Ok(roots[0].clone())
}
