//! THE SHELL'S CALLS ARE CONTINUATIONS (#795, everything async).
//!
//! A step that reads or writes through the kernel sends its call, gets the ACK at
//! once, and does the rest of its work INSIDE the continuation that runs with the
//! settle. Nothing awaits a settle: no oneshot, no channel, no timeout. A flow that
//! has an answer for its caller takes a [`Done`] and calls it, exactly once, from
//! whichever step ends the flow (the settle of its last call, or the refusal of one
//! that never launched).
//!
//! A handler of a bundle that answers from such a flow defers its settle
//! ([`reply_later`]): it returns at once, and the flow's `done` settles it.

use std::future::Future;
use std::sync::Arc;

use fantastic_kernel::{Kernel, Outcome};

use crate::kern::host::Host;
use futures_util::future::BoxFuture;
use serde_json::Value;

/// What a flow does with its answer: runs once, when the flow ends.
pub type Done<T> = Box<dyn FnOnce(T) -> BoxFuture<'static, ()> + Send>;

/// A [`Done`] from an async closure.
pub fn done<T, F, Fut>(f: F) -> Done<T>
where
    F: FnOnce(T) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Box::new(move |v| Box::pin(f(v)))
}

/// A bundle handler's answer from a flow: the handler DEFERS its settle, starts
/// `flow` with the `done` that settles it, and returns at once (its own return is
/// ignored). The settle is the reply `done` is given (a refusal-shaped one settles
/// `errored`). Outside a dispatch there is nothing to defer: the refusal is the answer.
pub async fn reply_later<F, Fut>(kernel: &Arc<Kernel>, flow: F) -> Value
where
    F: FnOnce(Done<Value>) -> Fut,
    Fut: Future<Output = ()>,
{
    let d = match kernel.defer() {
        Ok(d) => d,
        Err(refused) => return refused,
    };
    let k = Arc::clone(kernel);
    flow(done(move |reply: Value| async move {
        k.settle_deferred(d.pid(), Outcome::of_reply(Some(d.pid()), reply));
    }))
    .await;
    Value::Null
}

/// Send each of `calls` to the root as the operator, one AFTER the previous settled (their
/// order matters); `done` gets every reply, in order, from the last settle.
pub fn chain(host: &Host, calls: Vec<Value>, done: Done<Vec<Value>>) -> BoxFuture<'static, ()> {
    let host = host.clone();
    Box::pin(async move { chain_from(host, calls.into_iter().collect(), Vec::new(), done).await })
}

fn chain_from(host: Host, mut calls: std::collections::VecDeque<Value>, mut replies: Vec<Value>, done: Done<Vec<Value>>) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        let Some(next) = calls.pop_front() else {
            return done(replies).await;
        };
        let h = host.clone();
        host.operator_then("kernel", next, move |out| async move {
            replies.push(out.result);
            chain_from(h, calls, replies, done).await;
        })
        .await;
    })
}

/// TEST SUPPORT: run a flow and hand its one answer to the test. The test waits on a
/// channel the flow's `done` signals (event-driven); the timeout is only a hang guard,
/// and it FAILS.
#[cfg(test)]
pub async fn run<T, F, Fut>(flow: F) -> T
where
    T: Send + 'static,
    F: FnOnce(Done<T>) -> Fut,
    Fut: Future<Output = ()>,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    flow(done(move |v| async move {
        let _ = tx.send(v);
    }))
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(60), rx)
        .await
        .expect("the flow never called its done (hang guard)")
        .expect("the flow dropped its done")
}

/// TEST SUPPORT: one call as the operator, its settle handed to the test.
#[cfg(test)]
pub async fn op(host: &Host, target: &str, payload: Value) -> Outcome {
    run(|d| host.operator_then(target, payload, move |out| async move { d(out).await })).await
}

/// TEST SUPPORT: one call as the code running here (its own, as `agent_then` makes it).
#[cfg(test)]
pub async fn agent_op(host: &Host, target: &str, payload: Value) -> Outcome {
    run(|d| host.agent_then(target, payload, move |out| async move { d(out).await })).await
}

/// TEST SUPPORT: one call as `asker`.
#[cfg(test)]
pub async fn asker_op(host: &Host, asker: &Option<fantastic_kernel::Asker>, target: &str, payload: Value) -> Outcome {
    run(|d| host.asker_then(asker, target, payload, move |out| async move { d(out).await })).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_done_runs_once_with_its_answer() {
        let n = run(|d| async move { d(7_u32).await }).await;
        assert_eq!(n, 7);
    }

    #[tokio::test]
    async fn a_chain_sends_each_call_after_the_previous_settled_and_answers_in_order() {
        let (h, dir) = crate::kern::backend::create_tests::host("cps-chain").await;
        let make = |id: &str| json!({"type": "create_agent", "parent_id": "kernel", "id": id, "handler_module": "fake_store.tools"});
        // the second names the first as its parent: it only works when the first settled
        let second = json!({"type": "create_agent", "parent_id": "first", "id": "second", "handler_module": "fake_store.tools"});
        let replies = run(|d| chain(&h, vec![make("first"), second], d)).await;
        assert_eq!(replies.len(), 2, "{replies:?}");
        assert!(replies.iter().all(|r| r.get("error").is_none()), "{replies:?}");
        assert!(h.kernel.exists("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_reply_with_no_dispatch_is_the_refusal_at_once() {
        let k = Arc::new(Kernel::new());
        let r = reply_later(&k, |_d| async { panic!("no flow without a dispatch") }).await;
        assert!(fantastic_kernel::is_refused(&r), "{r}");
    }
}
