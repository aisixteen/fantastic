//! The TASK TABLE — `ps` + `cancel`, the kernel's process table.
//!
//! The unit is a **task**: ONE in-flight dispatch, not an agent. Its id
//! (`"<agent_id>:<n>"`) is a pid — `n` is a per-agent counter, monotonic for
//! the process lifetime, so an id is never reused and a stale id cancels
//! nothing rather than killing an innocent task.
//!
//! Tasks form a **tree** through the send chain: a `send` issued INSIDE a
//! handler is a CHILD of that handler's task. The current id rides the
//! [`CURRENT_TASK`] task-local exactly as the sender rides
//! [`crate::send::CURRENT_SENDER`] — and because `tokio::spawn` MOVES the
//! future onto a fresh task (task-locals do NOT inherit), the scope is
//! re-established INSIDE the spawned dispatch (see [`Kernel::run_dispatch`]).
//! That tree is what makes `cancel` a process-GROUP kill rather than a single
//! abort.
//!
//! ## Cancellation is two-phase (SIGTERM → SIGKILL)
//!
//! Dropping a future frees NO real resource — it unwinds `async` frames and
//! nothing else. An OS subprocess, a PTY, a socket held for that task must be
//! released by its bundle first. So [`Kernel::cancel_task`]:
//!
//! 1. **`on_cancel`** ([`crate::Bundle::on_cancel`]) — awaited for the WHOLE
//!    subtree while every task is still alive, so a bundle can release what it
//!    owns and no handler observes a half-killed group.
//! 2. **abort** — leaves-up, synchronously (no `await` in that loop): a parent
//!    can never resume between its child's death and its own.
//!
//! ## How a task is stopped
//!
//! Not `JoinHandle::abort` — that handle only exists AFTER `tokio::spawn`,
//! which would leave a window where a freshly-registered row is unkillable and
//! a nested `send` could claim a parent that isn't in the table yet. Instead a
//! [`TaskHandle`] (a `Notify` + a killed flag) is created BEFORE the spawn and
//! registered with the row; the dispatch runs in a `select!` against it, so
//! `stop()` drops the dispatch future at its next yield — abort semantics, no
//! race.

use crate::agent::AgentId;
use crate::kernel::Kernel;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio::task_local;

/// A task id — `"<agent_id>:<n>"`. A pid.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub String);

impl TaskId {
    /// Borrow the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TaskId {
    fn from(s: &str) -> Self {
        TaskId(s.to_string())
    }
}

impl From<String> for TaskId {
    fn from(s: String) -> Self {
        TaskId(s)
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

task_local! {
    /// The id of the task whose dispatch is running right now. Set INSIDE the
    /// spawned dispatch (a spawn moves the future — task-locals don't inherit),
    /// read by the next `send` to record its PARENT. That is what makes the
    /// table a tree. Unset outside any dispatch (a web surface, a one-shot CLI
    /// verb, a test) — such a task is a root of the table, like a process
    /// spawned by init.
    pub static CURRENT_TASK: TaskId;

    /// "My own task is inside a subtree the kernel is killing." Set by
    /// [`Kernel::cancel_task`] on every marked row BEFORE any task is stopped,
    /// so a handler that gets woken by its child's death does not resurrect
    /// itself: it parks and takes its own stop signal instead.
    pub static CURRENT_KILLED: Arc<AtomicBool>;
}

/// The current task id, or `None` outside any dispatch.
pub fn current_task() -> Option<TaskId> {
    CURRENT_TASK.try_with(|t| t.clone()).ok()
}

/// `true` iff the calling task has been marked killed by an in-flight
/// [`Kernel::cancel_task`]. `false` outside any dispatch.
pub fn current_killed() -> bool {
    CURRENT_KILLED
        .try_with(|f| f.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Seconds since the epoch — the `started_at` clock (Python's `time.time()`).
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The stop handle of one registered task: a `Notify` the task's `select!`
/// races against, plus the killed flag its own descendants read.
///
/// Created BEFORE the task is spawned so the row is complete (and killable)
/// the instant it enters the table.
#[derive(Clone)]
pub struct TaskHandle {
    cancel: Arc<Notify>,
    killed: Arc<AtomicBool>,
}

impl Default for TaskHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskHandle {
    /// A fresh handle for a task that is about to be registered + spawned.
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(Notify::new()),
            killed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Phase ① — mark the task as dying. Its own `send`s, woken by a child's
    /// death, will park instead of resuming.
    fn mark(&self) {
        self.killed.store(true, Ordering::SeqCst);
    }

    /// Phase ② — stop the task: the `select!` in `Kernel::run_dispatch` /
    /// [`Kernel::spawn_task`] takes this branch and DROPS the work future.
    /// `notify_one` stores a permit, so a task that hasn't been polled yet
    /// still stops the moment it starts.
    pub(crate) fn stop(&self) {
        self.cancel.notify_one();
    }

    /// The `Notify` the task's `select!` waits on.
    pub(crate) fn cancel_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.cancel)
    }

    /// The killed flag scoped into the task as [`CURRENT_KILLED`].
    pub(crate) fn killed_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.killed)
    }
}

/// One row of the table. `handle` is the stop handle and never leaves the
/// kernel — `ps` serves the five public fields only.
struct TaskEntry {
    agent: AgentId,
    parent: Option<TaskId>,
    verb: String,
    started_at: f64,
    /// Registration order — `ps` reports oldest first, deterministically (the
    /// `started_at` clock is too coarse to sort by).
    order: u64,
    handle: TaskHandle,
}

struct Inner {
    tasks: HashMap<TaskId, TaskEntry>,
    /// Per-agent task counter — the non-reuse guarantee. Hashed out of view by
    /// [`TaskTable::mint`]; never surfaced, because a visible counter reads as a
    /// sequence and messaging is async.
    seq: HashMap<AgentId, u64>,
    order: u64,
    /// Per-table, never persisted — makes the minted token opaque across runs.
    salt: u64,
}

impl Default for Inner {
    fn default() -> Self {
        // Address-derived: no RNG dependency in this crate, and the value only has to
        // be unpredictable-enough and per-process. Ids are process-local and never
        // persisted, so this is not a security boundary.
        let seed = Box::into_raw(Box::new(0u8));
        let salt = seed as u64 ^ 0x9e37_79b9_7f4a_7c15;
        // SAFETY: the allocation was just made by `Box::new` and is not used again.
        unsafe { drop(Box::from_raw(seed)) };
        Self {
            tasks: HashMap::new(),
            seq: HashMap::new(),
            order: 0,
            salt,
        }
    }
}

/// The live table of in-flight work. Lives on the [`Kernel`]; `ps` reads it,
/// `cancel` acts on it.
///
/// A plain `Mutex` — every critical section is a few map operations with no
/// `await` inside (the `on_cancel` hooks in [`Kernel::cancel_task`] run
/// between locked sections, never under the lock).
#[derive(Default)]
pub struct TaskTable {
    inner: Mutex<Inner>,
}

impl TaskTable {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Next task id for `agent_id` — `"<agent_id>:<8 lowercase hex>"`.
    ///
    /// OPAQUE on purpose. A visible counter reads as a SEQUENCE, and messaging here is
    /// fully async: a send may return once, several times, or never, and replies arrive
    /// out of order. Measured 2026-08-20 (python): with `<agent>:<n>` a model inferred
    /// ordering from the number, decided "the reply queue is running ahead of me", and
    /// re-polled forever instead of matching on the id. The monotonic `seq` is KEPT —
    /// it is what guarantees non-reuse — but it is hashed out of view, and a collision
    /// with a live row re-rolls rather than aliasing two dispatches onto one id.
    ///
    /// The salt is per-table and never persisted, so ids are unguessable across runs and
    /// carry no meaning outside this process. Matching the SHAPE is what parity needs;
    /// the hash itself is an implementation detail (python uses keyed blake2b).
    pub fn mint(&self, agent_id: &AgentId) -> TaskId {
        let mut inner = self.inner.lock().expect("task table poisoned");
        loop {
            let n = inner.seq.entry(agent_id.clone()).or_insert(0);
            *n += 1;
            let n = *n;
            let mut h = DefaultHasher::new();
            inner.salt.hash(&mut h);
            agent_id.0.hash(&mut h);
            n.hash(&mut h);
            let id = TaskId(format!("{}:{:08x}", agent_id.0, h.finish() as u32));
            if !inner.tasks.contains_key(&id) {
                return id;
            }
        }
    }

    /// Put one in-flight task in the table.
    ///
    /// A `parent` that is no longer in the table is dropped to `None` — the
    /// task is a root, adopted by init. Not defensive slack: a bundle's
    /// long-lived work outlives the dispatch that forked it, and a `send` it
    /// issues later must not claim a dead parent or the `ps` tree would lie.
    pub fn register(
        &self,
        id: &TaskId,
        agent_id: &AgentId,
        verb: &str,
        parent: Option<TaskId>,
        handle: TaskHandle,
    ) {
        let mut inner = self.inner.lock().expect("task table poisoned");
        let parent = parent.filter(|p| inner.tasks.contains_key(p));
        inner.order += 1;
        let order = inner.order;
        inner.tasks.insert(
            id.clone(),
            TaskEntry {
                agent: agent_id.clone(),
                parent,
                verb: verb.to_string(),
                started_at: now_secs(),
                order,
                handle,
            },
        );
    }

    /// Drop one row. Surviving children are RE-PARENTED to the dead task's
    /// parent — init-adoption: work forked out of a dispatch outlives it and
    /// must stay in the caller's process group, else cancelling the caller
    /// could never reach it. No-op if absent.
    pub fn forget(&self, id: &TaskId) {
        let mut inner = self.inner.lock().expect("task table poisoned");
        remove_and_reparent(&mut inner, id);
    }

    /// `true` iff `id` names a live task.
    pub fn contains(&self, id: &TaskId) -> bool {
        self.inner
            .lock()
            .expect("task table poisoned")
            .tasks
            .contains_key(id)
    }

    /// The public table — the `ps` reply rows `{id, agent, parent, verb,
    /// started_at}`, oldest first. The stop handle stays behind.
    pub fn rows(&self) -> Vec<Value> {
        let inner = self.inner.lock().expect("task table poisoned");
        let mut rows: Vec<(u64, Value)> = inner
            .tasks
            .iter()
            .map(|(id, e)| {
                (
                    e.order,
                    json!({
                        "id": id.0,
                        "agent": e.agent.0,
                        "parent": e.parent.as_ref().map(|p| p.0.clone()),
                        "verb": e.verb,
                        "started_at": e.started_at,
                    }),
                )
            })
            .collect();
        rows.sort_by_key(|(order, _)| *order);
        rows.into_iter().map(|(_, row)| row).collect()
    }

    /// The agent that owns `id`, if it is still live.
    pub fn agent_of(&self, id: &TaskId) -> Option<AgentId> {
        self.inner
            .lock()
            .expect("task table poisoned")
            .tasks
            .get(id)
            .map(|e| e.agent.clone())
    }

    /// Phase ① of `cancel`: `id` + every descendant, POST-ORDER (leaves
    /// first — the kill order of a process group), with every one of them
    /// MARKED killed before the caller runs a single hook or stops a single
    /// task. `None` if `id` isn't in the table.
    fn begin_cancel(&self, id: &TaskId) -> Option<Vec<TaskId>> {
        let inner = self.inner.lock().expect("task table poisoned");
        if !inner.tasks.contains_key(id) {
            return None;
        }
        let mut kids: HashMap<&TaskId, Vec<&TaskId>> = HashMap::new();
        for (tid, e) in inner.tasks.iter() {
            if let Some(p) = e.parent.as_ref() {
                kids.entry(p).or_default().push(tid);
            }
        }
        // Deterministic order among siblings (oldest first).
        for v in kids.values_mut() {
            v.sort_by_key(|t| inner.tasks.get(*t).map(|e| e.order).unwrap_or(0));
        }
        let mut out: Vec<TaskId> = Vec::new();
        walk(id, &kids, &mut out);
        for tid in &out {
            if let Some(e) = inner.tasks.get(tid) {
                e.handle.mark();
            }
        }
        Some(out)
    }

    /// Phase ② of `cancel`: stop the whole subtree leaves-up and drop its rows
    /// — SYNCHRONOUS, no `await`, so a parent cannot resume between its child's
    /// death and its own. Returns the ids actually killed (the table only ever
    /// holds LIVE work).
    fn abort_subtree(&self, ids: &[TaskId]) -> Vec<String> {
        let mut inner = self.inner.lock().expect("task table poisoned");
        let mut killed = Vec::new();
        for tid in ids {
            if let Some(entry) = remove_and_reparent(&mut inner, tid) {
                entry.handle.stop();
                killed.push(tid.0.clone());
            }
        }
        killed
    }
}

/// Post-order DFS of the task tree.
fn walk(id: &TaskId, kids: &HashMap<&TaskId, Vec<&TaskId>>, out: &mut Vec<TaskId>) {
    if let Some(children) = kids.get(id) {
        for c in children {
            walk(c, kids, out);
        }
    }
    out.push(id.clone());
}

/// Remove one row, re-parenting its surviving children to its parent.
fn remove_and_reparent(inner: &mut Inner, id: &TaskId) -> Option<TaskEntry> {
    let entry = inner.tasks.remove(id)?;
    for e in inner.tasks.values_mut() {
        if e.parent.as_ref() == Some(id) {
            e.parent = entry.parent.clone();
        }
    }
    Some(entry)
}

/// Drops one row when the task it belongs to ends — completed OR stopped.
/// Held INSIDE a task spawned by [`Kernel::spawn_task`], so the row lives
/// exactly as long as the work.
struct ForgetOnDrop {
    kernel: Arc<Kernel>,
    id: TaskId,
}

impl Drop for ForgetOnDrop {
    fn drop(&mut self) {
        self.kernel.tasks.forget(&self.id);
    }
}

impl Kernel {
    /// Register BACKGROUND work a bundle forks out of a dispatch (an OS
    /// subprocess pump, a PTY reader) — work that OUTLIVES the dispatch that
    /// started it. Bundles call this so their real work is visible to `ps` and
    /// killable by `cancel`; without it the dispatch returns, its row drops,
    /// and the resource is unreachable (that is the orphan bug).
    ///
    /// Parent = the dispatch that forked it, so the fork stays inside the
    /// caller's process group even after that dispatch returns (its row is
    /// re-parented to init on removal). Inside the spawned task the new task id
    /// is scoped, so anything the work `send`s nests under IT.
    ///
    /// Returns the new task id.
    pub fn spawn_task<F>(self: &Arc<Self>, agent_id: &AgentId, verb: &str, work: F) -> TaskId
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let task_id = self.tasks.mint(agent_id);
        let handle = TaskHandle::new();
        self.tasks
            .register(&task_id, agent_id, verb, current_task(), handle.clone());

        let cancel = handle.cancel_signal();
        let killed = handle.killed_flag();
        let guard = ForgetOnDrop {
            kernel: Arc::clone(self),
            id: task_id.clone(),
        };
        let tid = task_id.clone();
        tokio::spawn(async move {
            let _guard = guard;
            CURRENT_TASK
                .scope(
                    tid,
                    CURRENT_KILLED.scope(killed, async move {
                        tokio::select! {
                            biased;
                            _ = cancel.notified() => {}
                            _ = work => {}
                        }
                    }),
                )
                .await;
        });
        task_id
    }

    /// Kill a task AND ITS SUBTREE — children first, like killing a process
    /// group. Two-phase, mirroring SIGTERM → SIGKILL:
    ///
    /// ① `on_cancel` — each agent's bundle hook releases the REAL resources it
    ///    holds for that task (an OS subprocess, a PTY, a socket). Dropping a
    ///    future frees NONE of those. Nothing is stopped in this phase, so no
    ///    handler can wake mid-walk and observe a half-killed group.
    /// ② stop — the tasks are dropped leaves-up, synchronously.
    ///
    /// Returns `{cancelled, killed:[task_id]}`. An unknown or already-finished
    /// id is not in the table → `{"cancelled": false, "killed": []}`: cancel
    /// fails honestly, it never pretends.
    pub async fn cancel_task(self: &Arc<Self>, task_id: &TaskId) -> Value {
        let Some(ids) = self.tasks.begin_cancel(task_id) else {
            return json!({"cancelled": false, "killed": []});
        };
        // ① SIGTERM — hooks first, subtree-wide, while everything is still alive.
        for tid in &ids {
            let Some(agent_id) = self.tasks.agent_of(tid) else {
                continue;
            };
            let Some(agent) = self.agents.get(&agent_id).map(|e| Arc::clone(&e)) else {
                continue;
            };
            let Some(hm) = agent.handler_module.as_deref() else {
                continue;
            };
            let Some(bundle) = self.bundles.get(hm) else {
                continue;
            };
            // A hook that fails is logged and the kill still happens: cancel is
            // not refusable.
            if let Err(e) = bundle.on_cancel(&agent.id, tid, self).await {
                tracing::warn!(agent = %agent.id, task = %tid, "on_cancel hook raised: {e}");
            }
        }
        // ② SIGKILL — leaves-up, no await in the loop.
        let killed = self.tasks.abort_subtree(&ids);
        json!({"cancelled": true, "killed": killed})
    }
}

#[cfg(test)]
pub(crate) mod tests;
