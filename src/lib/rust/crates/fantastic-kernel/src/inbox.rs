//! The agent inbox — a bounded queue that drops the OLDEST payload when full.
//!
//! WHY NOT `tokio::mpsc`. A channel can only refuse from the sender side: when
//! it is full `try_send` fails and the payload being sent is lost. That is the
//! opposite of what python and swift do — python's `_put_drop_oldest` evicts the
//! head, swift's `AsyncStream` uses `.bufferingNewest` — and the rule they share
//! is the one a stream consumer needs: under backpressure the FRESHEST events
//! survive, because a token stream or a PTY redraw is only useful at the tip.
//!
//! Rust drifted for years with a comment claiming the behaviours matched. They
//! did not: a slow consumer kept an old head and silently discarded everything
//! new, so a stalled tab froze on stale frames instead of catching up.
//!
//! An inbox exists iff a READER registered it (`Kernel::ensure_inbox`); nothing
//! auto-vivifies one, so every queue here has someone draining it.

use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

struct Shared {
    queue: Mutex<VecDeque<Value>>,
    ready: Notify,
    bound: usize,
}

/// The write half. Cloneable, never blocks, never fails.
#[derive(Clone)]
pub struct InboxSender(Arc<Shared>);

/// The read half. One per inbox — the reader that registered it.
pub struct InboxReceiver(Arc<Shared>);

impl InboxSender {
    /// Append. At the bound the OLDEST payload is evicted to make room, so the
    /// newest always lands.
    pub fn send(&self, v: Value) {
        {
            let mut q = self.0.queue.lock().expect("inbox poisoned");
            if q.len() >= self.0.bound {
                q.pop_front();
            }
            q.push_back(v);
        }
        self.0.ready.notify_one();
    }
}

impl InboxReceiver {
    /// The next payload, waiting if the queue is empty.
    pub async fn recv(&mut self) -> Option<Value> {
        loop {
            if let Some(v) = self.0.queue.lock().expect("inbox poisoned").pop_front() {
                return Some(v);
            }
            self.0.ready.notified().await;
        }
    }

    /// The next payload if one is already queued.
    pub fn try_recv(&mut self) -> Option<Value> {
        self.0.queue.lock().expect("inbox poisoned").pop_front()
    }
}

/// One inbox, bounded at `bound` payloads.
#[must_use]
pub fn inbox(bound: usize) -> (InboxSender, InboxReceiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        ready: Notify::new(),
        bound,
    });
    (InboxSender(Arc::clone(&shared)), InboxReceiver(shared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn the_oldest_is_evicted_so_the_newest_always_lands() {
        let (tx, mut rx) = inbox(3);
        for i in 0..5 {
            tx.send(json!({ "n": i }));
        }
        // 0 and 1 were pushed out; the tip survives.
        let got: Vec<i64> = (0..3)
            .map(|_| rx.try_recv().expect("three held")["n"].as_i64().unwrap())
            .collect();
        assert_eq!(got, vec![2, 3, 4], "a full inbox must drop the OLDEST");
        assert!(rx.try_recv().is_none());
    }

    #[tokio::test]
    async fn a_waiting_reader_is_woken() {
        let (tx, mut rx) = inbox(8);
        let h = tokio::spawn(async move { rx.recv().await });
        tokio::task::yield_now().await;
        tx.send(json!({"n": 1}));
        assert_eq!(h.await.unwrap().unwrap()["n"], 1);
    }
}
