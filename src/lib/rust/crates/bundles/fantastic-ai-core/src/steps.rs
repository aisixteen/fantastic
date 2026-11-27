//! A PASS'S PLUMBING AS STEPS (#795, plan 2.3): every call an AI agent makes to another
//! agent on its message path — its memory store, its own `reflect`, its provider's
//! `capabilities`, a file bridge's bytes — goes out through [`Kernel::call`], and its
//! settle is the NEXT STEP, run as a message to the agent. A step never awaits another
//! agent: it hands what the next step needs over in a record, sends its call, and ENDS.
//!
//! - [`call`]: one call; its settle (or its refusal at launch, at once) runs the next
//!   step with the record.
//! - [`store`]: one call to the agent's memory store, its reply checked as every store
//!   reply is ([`crate::memory::checked`]).
//! - [`append`] / [`history`]: the memory layer's write-through append and head read as
//!   steps ([`crate::memory::save_history`] and [`crate::memory::load_history`]'s rules).
//! - [`own_reflect`]: the agent's own `reflect`, as the system block's self part reads it.
//!
//! A continuation lives in RAM: a restart loses it, and its settle then arrives as a
//! plain `reply` with an unknown `re`, which wakes nothing; the messages a lost pass
//! would have read are stored, and the boot reads them ([`crate::life::restore_unread`]).

use std::future::Future;
use std::sync::{Arc, Mutex};

use fantastic_kernel::{AgentId, Item, Kernel, Outcome, Status};
use serde_json::{json, Value};

use crate::memory::MemResult;

/// ONE PLUMBING CALL, as a step: `item` goes out through [`Kernel::call`], and its
/// settle runs `next` with `rec`, the record the step hands on. A call refused at its
/// launch (the gate, an unknown target) runs `next` at once with the refusal. The step
/// that calls this ends when it returns; nothing waits for the settle.
pub(crate) async fn call<R, F, Fut>(kernel: &Arc<Kernel>, item: Item, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Outcome) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    // the record and the next step, taken by whichever comes: the settle, or the refusal
    let slot = Arc::new(Mutex::new(Some((rec, next))));
    let held = Arc::clone(&slot);
    let ack = kernel
        .call(item, move |out| async move {
            let taken = held.lock().expect("step poisoned").take();
            if let Some((rec, next)) = taken {
                next(rec, out).await;
            }
        })
        .await;
    if ack.status != Status::Accepted {
        let taken = slot.lock().expect("step poisoned").take();
        if let Some((rec, next)) = taken {
            next(rec, ack).await;
        }
    }
}

/// ONE CALL FOR THE ASKER of the verb a step belongs to, as a step
/// ([`Kernel::call_for`]): sent and checked as `asker`; its settle (or its refusal at
/// launch) runs `next` with `rec`, as this agent.
pub(crate) async fn call_for<R, F, Fut>(kernel: &Arc<Kernel>, asker: &fantastic_kernel::Asker, item: Item, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Outcome) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let slot = Arc::new(Mutex::new(Some((rec, next))));
    let held = Arc::clone(&slot);
    let ack = kernel
        .call_for(asker, item, move |out| async move {
            let taken = held.lock().expect("step poisoned").take();
            if let Some((rec, next)) = taken {
                next(rec, out).await;
            }
        })
        .await;
    if ack.status != Status::Accepted {
        let taken = slot.lock().expect("step poisoned").take();
        if let Some((rec, next)) = taken {
            next(rec, ack).await;
        }
    }
}

/// ONE CALL TO THE AGENT'S OWN MEMORY STORE, as a step: `payload` (and the request
/// body at `data`, for the byte verbs) to the store the record names, as the agent; the
/// next step gets the reply checked — a missing store, a refusal or a failed dispatch is
/// the `Err` — and the reply body beside it.
pub(crate) async fn store<R, F, Fut>(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    payload: Value,
    body: Option<Vec<u8>>,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, MemResult<crate::memory::StoreReply>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (item, sid, verb) = match crate::memory::store_request(self_id, kernel, payload, body) {
        Ok(r) => r,
        Err(e) => return next(rec, Err(e)).await,
    };
    let me = self_id.clone();
    call(kernel, item, rec, move |rec, out| async move {
        next(rec, crate::memory::checked(&me, &sid, &verb, out)).await;
    })
    .await;
}

/// APPEND `rows` to the agent's memory store as a step, then to its head cache
/// (write-through, [`crate::memory::save_history`]'s rules): the next step gets the ids.
pub(crate) async fn append<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rows: &[Value], rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<i64>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let rows = match crate::memory::rows_to_append(self_id, kernel, rows) {
        Ok(r) => r,
        Err(e) => return next(rec, Err(e)).await,
    };
    if rows.is_empty() {
        return next(rec, Ok(Vec::new())).await;
    }
    let payload = crate::memory::append_payload(&rows);
    let (me, k) = (self_id.clone(), Arc::clone(kernel));
    store(self_id, kernel, payload, None, rec, move |rec, r| async move {
        next(rec, r.map(|_| crate::memory::appended(&me, &k, rows))).await;
    })
    .await;
}

/// THE AGENT'S CONVERSATION FROM ITS HEAD, as a step ([`crate::memory::load_history`]
/// with `behind = false`): the head read from the store the first time, else the rows
/// after its newest merged in; the next step gets the rows.
pub(crate) async fn history<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<Value>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    refreshed(self_id, kernel, rec, move |rec, r| next(rec, r.map(crate::memory::chat_rows))).await;
}

/// THE HEAD BROUGHT UP TO THE STORE, as a step ([`crate::memory::refresh_head`]): read
/// whole the first time, else the rows after its newest merged in; the next step gets
/// the head's rows, bookkeeping and card rows included.
pub(crate) async fn refreshed<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<Value>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let read = crate::memory::head_read(self_id, kernel);
    let (me, k) = (self_id.clone(), Arc::clone(kernel));
    store(self_id, kernel, read.payload(), None, rec, move |rec, r| async move {
        next(rec, r.map(|(res, _)| read.merge(&me, &k, &res))).await;
    })
    .await;
}

/// THE AGENT'S OWN `reflect` (`tree: none`), as a step: what the system block's self
/// part is cut from ([`crate::assembly::self_text`]).
pub(crate) async fn own_reflect<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, Value) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let item = Item::new(self_id.clone(), json!({"type": "reflect", "tree": "none"}));
    call(kernel, item, rec, move |rec, out| next(rec, out.result)).await;
}

/// THE AGENT'S WHOLE CONVERSATION, as a step ([`crate::memory::load_history`] with
/// `behind = true`): every row the store holds, before a `clear` too; the next step
/// gets the rows.
pub(crate) async fn whole<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<Value>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    rows(self_id, kernel, json!({"type": "read"}), rec, move |rec, r| next(rec, r.map(crate::memory::chat_rows)))
        .await;
}

/// ONE STORE READ, as a step: the rows the store answers `payload` with, each tagged
/// with the agent ([`crate::memory::rows_of`]).
pub(crate) async fn rows<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, payload: Value, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<Value>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let me = self_id.clone();
    store(self_id, kernel, payload, None, rec, move |rec, r| async move {
        next(rec, r.map(|(res, _)| crate::memory::rows_of(&res, &me))).await;
    })
    .await;
}

/// THE HEAD CACHE, as a step ([`crate::memory::head`]): the cached head when there is
/// one, else read whole from the store (from its last `clear`).
pub(crate) async fn head<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<Value>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if let Some(h) = crate::memory::cached_head(self_id, kernel) {
        return next(rec, Ok(h)).await;
    }
    let read = crate::memory::HeadRead::Whole;
    let (me, k) = (self_id.clone(), Arc::clone(kernel));
    store(self_id, kernel, read.payload(), None, rec, move |rec, r| async move {
        next(rec, r.map(|(res, _)| read.merge(&me, &k, &res))).await;
    })
    .await;
}

/// A HANDLER THAT ANSWERS LATER (#795): its execution defers, and `settle` is how its
/// steps settle it — once, from whichever step ends the work. `None` in a kernel
/// bookkeeping run, which answers no call: the steps run, nothing settles.
#[must_use]
pub fn deferred(kernel: &Arc<Kernel>) -> Settle {
    Settle {
        kernel: Arc::clone(kernel),
        d: kernel.defer().ok(),
    }
}

/// How a deferred handler settles ([`deferred`]).
pub struct Settle {
    kernel: Arc<Kernel>,
    d: Option<fantastic_kernel::Deferred>,
}

impl Settle {
    /// Settle the handler's execution with `result`: an `Err` is the raise its error
    /// makes (`failed`, `{error, raised}`).
    pub(crate) fn with<E: Into<fantastic_kernel::bundle::BundleError>>(self, result: Result<Value, E>) {
        crate::verbs::settle_with(&self.kernel, self.d, result);
    }

    /// Settle the handler's execution with its reply, as a handler's returned reply
    /// settles: `errored` when it is a refusal, else `completed`.
    pub fn reply(self, v: Value) {
        let Some(d) = self.d else { return };
        let status = if fantastic_kernel::is_refused(&v) {
            Status::Errored
        } else {
            Status::Completed
        };
        self.kernel
            .settle_deferred(d.pid(), Outcome::new(Some(d.pid()), status, v));
    }

    /// Settle the handler's execution with its reply and the reply's body (the bytes a
    /// reply carries), as [`Self::reply`] settles.
    pub fn reply_with(self, v: Value, body: Option<fantastic_kernel::Blob>) {
        let Some(d) = self.d else { return };
        let status = if fantastic_kernel::is_refused(&v) {
            Status::Errored
        } else {
            Status::Completed
        };
        self.kernel
            .settle_deferred(d.pid(), Outcome::new(Some(d.pid()), status, v).with_body(body));
    }

    /// The deferred execution (`None`: a kernel bookkeeping run, nothing deferred):
    /// what a settle that comes from elsewhere later names
    /// ([`Kernel::settle_deferred`]).
    #[must_use]
    pub fn pid(&self) -> Option<fantastic_kernel::TaskId> {
        self.d.map(|d| d.pid())
    }

    /// Where the handler's own I/O runs after a step ([`Forker::run`]).
    #[must_use]
    pub fn forker(&self) -> Forker {
        Forker {
            kernel: Arc::clone(&self.kernel),
            pid: self.d.map(|d| d.pid()),
        }
    }
}

/// THE HANDLER'S OWN I/O, after a step (#795): [`Forker::run`].
#[derive(Clone)]
pub struct Forker {
    kernel: Arc<Kernel>,
    pid: Option<fantastic_kernel::TaskId>,
}

impl Forker {
    /// `work` runs as a task of `owner`'s under the deferred execution, so its `cancel`
    /// and its deadline end the work with it (an HTTP stream, a socket). An execution
    /// already ended runs nothing; a kernel bookkeeping run (nothing deferred) runs
    /// `work` as its own task.
    pub fn run<W>(&self, owner: &AgentId, verb: &str, work: W)
    where
        W: Future<Output = ()> + Send + 'static,
    {
        match self.pid {
            Some(pid) if self.kernel.tasks.contains(&pid) => {
                let k = Arc::clone(&self.kernel);
                fantastic_kernel::CURRENT_TASK.sync_scope(pid, || {
                    k.spawn_task(owner, verb, work);
                });
            }
            Some(_) => {}
            None => {
                fantastic_kernel::spawn_inherit(work);
            }
        }
    }
}

/// THE AGENT'S VIEW WRITTEN, as a step ([`crate::memory::save_view`]): the folds and the
/// view row in one append; the next step gets the map of the folds' ids (provisional ->
/// minted).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn save_view<R, F, Fut>(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    folds: &[Value],
    ids: &[Value],
    budget: i64,
    why: &str,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, MemResult<std::collections::BTreeMap<i64, i64>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (rows, map) = crate::memory::view_rows(kernel, folds, ids, budget, why);
    append(self_id, kernel, &rows, rec, move |rec, r| next(rec, r.map(|_| map))).await;
}
