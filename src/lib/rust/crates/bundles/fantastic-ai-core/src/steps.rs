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
/// after its newest merged in; the next step gets the turns.
pub(crate) async fn history<R, F, Fut>(self_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<Vec<Value>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let read = crate::memory::head_read(self_id, kernel);
    let (me, k) = (self_id.clone(), Arc::clone(kernel));
    store(self_id, kernel, read.payload(), None, rec, move |rec, r| async move {
        let rows = r.map(|(res, _)| crate::memory::turns(read.merge(&me, &k, &res)));
        next(rec, rows).await;
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
