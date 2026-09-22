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
use crate::tasks::{TaskId, TaskTable};

/// The loader's durable write-through, kicked on every settle with that
/// execution's CALL ROW id (python passes `d.pid`).
pub type LedgerFlusher = Box<dyn Fn(Option<TaskId>) + Send + Sync>;
use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Notify;

/// Bounded inbox capacity per agent — matches Python's `INBOX_BOUND`.
/// 256 fits a chat-paced workload comfortably and bounds memory on a
/// runaway emitter. Tunable via [`Kernel::new_with_inbox_bound`].
pub const DEFAULT_INBOX_BOUND: usize = 256;

/// A subscriber callback fired on every state event (`send` / `emit`
/// / `removed` / etc.). Synchronous, called inline — keep it cheap;
/// off-thread the work if it might block.
///
/// The second argument is the event payload's RAW BODY — `(dotted path within
/// `payload`, bytes)` — for a traffic event whose payload carried bytes. Python
/// queues the bytes inside the payload dict and its WS tap re-encodes the whole
/// event as a binary frame; a `serde_json::Value` cannot hold bytes, so the body
/// rides beside the event exactly as it rides beside an
/// [`crate::inbox::InboxItem`]. `None` for every event with no bytes.
pub type StateSubscriber = Arc<dyn Fn(&Value, Option<&(String, Vec<u8>)>) + Send + Sync>;

/// The registry of state taps, SHARED. The kernel publishes through it — and so
/// does the ROW STORE: an append is a state event (`kind: "row"`), and
/// [`crate::rows::RowStore::put`] is the one choke point every append passes
/// through, exactly as python's `Kernel.rows_put` is.
pub(crate) type StateSubs = RwLock<Vec<(SubscriberToken, StateSubscriber)>>;

/// [`Kernel::publish_state_with_body`]'s body, against a bare registry so a
/// holder that is not the kernel (the row store) publishes the same way.
///
/// `ts` is stamped HERE and LAST (python `{**event, "ts": time.time()}`), so
/// every event carries the moment it was published and no caller has to
/// remember to add one. No subscribers = no work at all, as in python.
pub(crate) fn publish_state_to(subs: &StateSubs, event: &Value, body: Option<&(String, Vec<u8>)>) {
    let subs = subs.read().expect("state subs poisoned");
    if subs.is_empty() {
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
fn call_state_subscriber(cb: &StateSubscriber, event: &Value, body: Option<&(String, Vec<u8>)>) {
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
    /// Stable name → agent id, wired from an agent's `alias` meta at
    /// `register` (and dropped at `unregister`) — python `Kernel.well_known`.
    /// `send` targets, `create_agent`'s `parent_id` and the gate's operand
    /// resolution all resolve a non-"kernel" id through this AFTER the
    /// `kernel`→root special case (C35), so a caller can address a well-known
    /// agent (e.g. `web_loader`) by that stable name instead of its minted id.
    pub well_known: DashMap<String, AgentId>,
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
    /// Graceful-shutdown signal. The root-only `shutdown_kernel` verb
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

    /// EVERY execution and what it became — named (a caller's dedup contract)
    /// or unnamed (under its minted pid). Session lifetime, retention-ringed;
    /// the DURABLE half is installed by the root loader, because the kernel
    /// never touches disk itself.
    pub ledger: crate::ledger::Ledger,
    /// THE MESSAGE STORE — every message the kernel ever made, id-addressed by
    /// the one counter. Conversations, reply bodies, agent views and the
    /// kernel's own `call`/`task` bookkeeping share one append-only table, so
    /// one stream carries everything. Starts EMPTY, always: a hydrator fills it
    /// from above and a persister tails it; the kernel never knows disk.
    pub rows: crate::rows::RowStore,

    /// Installed by the ROOT LOADER at boot. `None` = no persistence (no store
    /// wired, or a test kernel): settled outcomes live and die with the process,
    /// exactly like the tree's own auto-flush law. Called at every settle.
    ledger_flusher: Mutex<Option<LedgerFlusher>>,
}

/// One bound for every agent's `shutdown` dispatch. Python
/// `Kernel.SHUTDOWN_VERB_TIMEOUT`.
const SHUTDOWN_VERB_TIMEOUT: f64 = 30.0;
/// How long in-flight work may take to settle before it is cancelled. Python
/// `Kernel.QUIET_TIMEOUT`.
const QUIET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
        // ONE registry, two publishers: the kernel and the row store, which
        // publishes the `row` event from inside its own append.
        let state_subscribers: Arc<StateSubs> = Arc::new(RwLock::new(Vec::new()));
        Self {
            agents: DashMap::new(),
            agent_order: RwLock::new(Vec::new()),
            well_known: DashMap::new(),
            inboxes: DashMap::new(),
            state_subscribers: Arc::clone(&state_subscribers),
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
            rows: crate::rows::RowStore::with_state(state_subscribers),
            ledger_flusher: Mutex::new(None),
        }
    }

    /// Signal the daemon serve loop to begin a graceful shutdown. Called
    /// by the root-only `shutdown_kernel` verb AFTER it has produced its
    /// ack reply, so the reply flushes over the web surface before the
    /// process exits. Idempotent — extra calls collapse to one shutdown
    /// (the serve loop awaits this once, then unwinds straight-line).
    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Resolves once [`Self::request_shutdown`] has been called. The CLI
    /// daemon awaits this in its `tokio::select!` next to SIGINT/SIGTERM;
    /// when it fires, the daemon drains agents, releases `lock.json`, and
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
            // `shutdown_context()`): probed or written while draining, the
            // store would answer "draining" — read as "no log" — and the header
            // was never written.
            if let Err(e) = crate::send::with_shutdown(bundle.flush_pending(&root.id, self)).await {
                eprintln!("  [shutdown] flush_pending raised: {e}");
            }
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
            if let Some(bundle) = agent
                .handler_module
                .as_deref()
                .and_then(|hm| self.bundles.get(hm))
            {
                // C87: python's exact text is "hook raised", not "on_shutdown
                // raised" (`_agent.py:2900-2906`, `print(f"  [shutdown]
                // {self.id} hook raised: {e}", file=sys.stderr)`) — the same
                // wording whether the hook resolved to `on_shutdown` or the
                // `on_delete` fallback.
                if let Err(e) = bundle.on_shutdown(&agent.id, self).await {
                    eprintln!("  [shutdown] {} hook raised: {e}", agent.id.0);
                }
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
    /// The kernel keeps serving afterwards — reads, `messages`, the persister —
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
            {
                let items: Vec<crate::Item> = targets
                    .iter()
                    .map(|id| {
                        crate::Item::new(id.clone(), json!({"type": "shutdown"}))
                            .with_timeout(SHUTDOWN_VERB_TIMEOUT)
                    })
                    .collect();
                // EXEMPT FROM THE DRAIN GATE: this IS the shutdown.
                let outs = crate::send::with_shutdown(self.send(items)).await;
                for (id, o) in targets.iter().zip(outs.iter()) {
                    if o.status == crate::outcome::Status::Completed {
                        shut.push(id.0.clone());
                    }
                }
            }
        }
        // The dispatch running THIS verb is in the table too; it is not
        // pending work.
        let me = crate::tasks::current_task();
        let deadline = std::time::Instant::now() + QUIET_TIMEOUT;
        // QUIET WAITS ON DISPATCHES ONLY — a forked service loop never settles.
        while std::time::Instant::now() < deadline && !self.tasks.pending_dispatches(me).is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let mut cancelled: Vec<Value> = Vec::new();
        for tid in self.tasks.pending(me) {
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
        json!({"drained": true, "shut": shut, "cancelled": cancelled})
    }

    /// Install the durable-ledger write-through. The kernel calls it after every
    /// settle and knows NOTHING about what it does — the loader owns the disk,
    /// exactly as it owns the `.fantastic` layout.
    pub fn set_ledger_flusher(&self, f: LedgerFlusher) {
        *self.ledger_flusher.lock().expect("flusher poisoned") = Some(f);
    }

    /// Fire the durable write-through, if one is installed. No flusher = no
    /// persistence: settled outcomes live and die with the process. That is the
    /// auto-flush law, not a fallback.
    /// `pid` is the settled execution's CALL ROW id — python passes `d.pid`.
    /// The loader's kick reads that row to tell its OWN persistence traffic from
    /// real work; the kernel still knows nothing about what persistence is.
    pub fn ledger_settled(&self, pid: Option<TaskId>) {
        let g = self.ledger_flusher.lock().expect("flusher poisoned");
        if let Some(f) = g.as_ref() {
            f(pid);
        }
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
        // C35: a declared `alias` meta becomes a stable, resolvable NAME —
        // `send` targets, `create_agent`'s `parent_id` and the gate's operand
        // resolution all go through `well_known` after the `kernel` special
        // case. Declared config the substrate makes FUNCTIONAL, not
        // autonomous behaviour. Python `Kernel.register`.
        if let Some(alias) = agent
            .meta
            .read()
            .expect("meta lock poisoned")
            .get("alias")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.well_known.insert(alias.to_string(), agent.id.clone());
        }
        self.agents.insert(agent.id.clone(), agent);
    }

    /// Resolve a `send`-style operand THROUGH `well_known`: a declared
    /// `alias` meta resolves to its agent id, a literal id resolves to
    /// itself — python `self.ctx.agents.get(self.ctx.well_known.get(x, x))`.
    /// Callers special-case the `"kernel"` alias to the root BEFORE calling
    /// this (C35 lands it after that check, never instead of it).
    #[must_use]
    pub fn resolve_well_known(&self, id: &str) -> AgentId {
        self.well_known
            .get(id)
            .map(|e| e.value().clone())
            .unwrap_or_else(|| AgentId::from(id))
    }

    /// Resolve a `send`-style operand end to end (C35): `"kernel"` aliases the
    /// root; otherwise a declared `alias` meta resolves through `well_known`,
    /// then a literal id. `None` when it names no live agent. The ONE place
    /// `send`'s target, `send_with_binary`'s target, `create_agent`'s
    /// `parent_id` and the gate's operand resolution all go through, so the
    /// four cannot drift on what "the same id" resolves to.
    #[must_use]
    pub fn resolve_target(&self, raw_id: &str) -> Option<Arc<Agent>> {
        if raw_id == "kernel" {
            return self.root();
        }
        let resolved = self.resolve_well_known(raw_id);
        self.agents.get(&resolved).map(|e| Arc::clone(&e))
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
        // Drop any alias pointing at the agent being removed (C35).
        self.well_known.retain(|_, target| target != id);
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
    pub fn publish_state_with_body(&self, event: &Value, body: Option<&(String, Vec<u8>)>) {
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

    /// [`Self::add_state_subscriber`] on behalf of a READER — a leg subscribing
    /// for a client, after `authorize_state_subscribe`.
    ///
    /// `row` events are then delivered only for agents that reader may read
    /// ([`Self::may_read_rows`]): the stream cannot leak a digest the `messages`
    /// verb would refuse. Every other kind passes through unchanged, and an
    /// operator tap ([`Self::add_state_subscriber`]) sees everything.
    ///
    /// `body` (after [`Self::authorize_row_bodies`] for that reader) adds the
    /// stored row's `content` to every `row` event, looked up by id at publish
    /// time, so a view that renders every row skips the pull round-trip. A
    /// `rows_read` holder reads every row whole already, so NO per-row filter
    /// applies to it.
    ///
    /// WEAK, not strong: the registry lives inside the kernel, so a gating
    /// closure holding an `Arc<Kernel>` would keep the kernel alive forever.
    pub fn add_state_subscriber_for(
        self: &Arc<Self>,
        cb: StateSubscriber,
        reader: AgentId,
        body: bool,
    ) -> SubscriberToken {
        let weak = Arc::downgrade(self);
        self.add_state_subscriber(Arc::new(move |event: &Value, raw| {
            if event.get("kind").and_then(Value::as_str) == Some("row") {
                let Some(kernel) = weak.upgrade() else {
                    return;
                };
                if body {
                    let content = event
                        .get("row")
                        .and_then(|r| r.get("id"))
                        .and_then(Value::as_i64)
                        .and_then(|id| kernel.rows.get_one(id))
                        .and_then(|row| row.get("content").cloned())
                        .unwrap_or_else(|| Value::from(""));
                    let mut event = event.clone();
                    if let Some(row) = event.get_mut("row").and_then(Value::as_object_mut) {
                        // LAST, as python's `{**row, "content": …}` — a view
                        // diffs these bytes.
                        row.insert("content".to_string(), content);
                    }
                    cb(&event, raw);
                    return;
                }
                let owner = event.get("agent_id").and_then(Value::as_str).unwrap_or("");
                if !kernel.may_read_rows(Some(&reader), owner) {
                    return;
                }
            }
            cb(event, raw);
        }))
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
    /// Pure read; walks the registry in REGISTRATION ORDER (python's dict
    /// order), so [`Self::save_json`] is byte-deterministic for equal kernel
    /// state. Ephemeral agents are skipped (they're per-process composition,
    /// never round-trip through a snapshot).
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
        let agents: Vec<_> = self
            .agent_ids()
            .into_iter()
            .filter(|id| !dropped.contains(id))
            .filter_map(|id| self.agents.get(&id).map(|e| e.value().record()))
            .collect();
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
    /// `persistence::load_children` semantics. The bootstrap caller
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

        // Validate: every parent_id must point at a record in the
        // snapshot, exactly one record must have parent_id == None
        // (the root), every id must be unique.
        let mut roots = 0;
        let mut ids_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for rec in &state.agents {
            // RESERVED KEYS ARE REFUSED HERE — before anything is torn down.
            // These name substrate wiring, not agent meta, and a hand-authored
            // `root_path:` once cleared the whole tree and THEN raised: the
            // kernel was bricked by a field that looked like configuration.
            // Validation before teardown is what makes a bad document a no-op.
            for k in crate::agent::RESERVED_RECORD_KEYS {
                if rec.meta.contains_key(k) {
                    return Err(KernelError::InvalidSnapshot(format!(
                        "record {:?} carries reserved key {k:?} — that names substrate \
                         wiring, not agent meta",
                        rec.id
                    )));
                }
            }
            if !ids_seen.insert(rec.id.as_str()) {
                return Err(KernelError::InvalidSnapshot(format!(
                    "duplicate agent id {:?} in snapshot",
                    rec.id
                )));
            }
            if rec.parent_id.is_none() {
                roots += 1;
            }
        }
        if roots == 0 {
            return Err(KernelError::InvalidSnapshot(
                "snapshot has no root (no record with parent_id == null)".to_string(),
            ));
        }
        if roots > 1 {
            return Err(KernelError::InvalidSnapshot(format!(
                "snapshot has {roots} roots; expected exactly one"
            )));
        }
        for rec in &state.agents {
            if let Some(pid) = &rec.parent_id {
                if !ids_seen.contains(pid.as_str()) {
                    return Err(KernelError::InvalidSnapshot(format!(
                        "agent {:?} parent {:?} not in snapshot",
                        rec.id, pid
                    )));
                }
            }
        }
        // REACHABILITY. Every parent resolving is not enough: a CYCLE
        // (a -> b -> a) resolves on both ends and is unreachable from the root,
        // so those records were dropped mid-hydration and reported afterwards —
        // by which point the old tree was already gone. Caught HERE, with the
        // other pre-teardown checks, so a bad snapshot is a no-op.
        {
            let mut kids: std::collections::HashMap<&str, Vec<&str>> =
                std::collections::HashMap::new();
            let mut root_id: &str = "";
            for rec in &state.agents {
                match &rec.parent_id {
                    Some(p) => kids.entry(p.as_str()).or_default().push(rec.id.as_str()),
                    None => root_id = rec.id.as_str(),
                }
            }
            let mut reached: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut stack = vec![root_id];
            while let Some(node) = stack.pop() {
                if !reached.insert(node) {
                    continue;
                }
                if let Some(cs) = kids.get(node) {
                    stack.extend(cs.iter().copied());
                }
            }
            let mut orphans: Vec<&str> = ids_seen
                .iter()
                .copied()
                .filter(|id| !reached.contains(id))
                .collect();
            if !orphans.is_empty() {
                orphans.sort_unstable();
                return Err(KernelError::InvalidSnapshot(format!(
                    "snapshot has {} record(s) unreachable from the root {root_id:?} \
                     (a cycle or an orphaned parent chain): {orphans:?}",
                    orphans.len()
                )));
            }
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
                    (Some(_), Some(pid)) => by_id
                        .get(pid)
                        .expect("parent processed before child")
                        .children_dir()
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
                let name = a.display_name().unwrap_or_else(|| a.id.0.clone());
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
