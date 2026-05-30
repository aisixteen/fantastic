//! In-process memory transport — paired channels used by tests.
//!
//! [`MemoryTransport::pair`] returns two halves whose `send_frame`
//! queues feed the other's `recv_frame` queue. Closing one half
//! signals the peer's `recv_frame` to resolve with
//! `ConnectionClosed`, matching how a real socket would behave.

use super::{BridgeTransport, TransportError};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Notify};

/// One half of an in-process bridge pair.
pub struct MemoryTransport {
    out: mpsc::Sender<Value>,
    inbox: Mutex<mpsc::Receiver<Value>>,
    /// Set when THIS half closes — used to short-circuit our own
    /// subsequent sends + to notify the peer's `recv_frame`.
    own_close: Arc<Notify>,
    /// Set when the PEER half closes — used by `recv_frame` to
    /// resolve with `ConnectionClosed` rather than block forever.
    peer_close: Arc<Notify>,
    /// Mirror flags so closed state is observable without an
    /// async wait. `recv_frame` checks these on every poll.
    own_closed: Arc<std::sync::atomic::AtomicBool>,
    peer_closed: Arc<std::sync::atomic::AtomicBool>,
}

impl MemoryTransport {
    /// Build two cross-wired halves. A's `send_frame` lands on B's
    /// `recv_frame` and vice versa. Closing either side immediately
    /// surfaces as `ConnectionClosed` on the other side's next
    /// `recv_frame` (or current pending one, via Notify).
    pub fn pair() -> (Arc<Self>, Arc<Self>) {
        // 64 is generous for tests — every frame is one JSON object
        // and the bridge round-trips never fan beyond a handful.
        let (tx_ab, rx_ab) = mpsc::channel::<Value>(64);
        let (tx_ba, rx_ba) = mpsc::channel::<Value>(64);
        let close_a = Arc::new(Notify::new());
        let close_b = Arc::new(Notify::new());
        let closed_a = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed_b = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let a = Arc::new(Self {
            out: tx_ab,
            inbox: Mutex::new(rx_ba),
            own_close: Arc::clone(&close_a),
            peer_close: Arc::clone(&close_b),
            own_closed: Arc::clone(&closed_a),
            peer_closed: Arc::clone(&closed_b),
        });
        let b = Arc::new(Self {
            out: tx_ba,
            inbox: Mutex::new(rx_ab),
            own_close: close_b,
            peer_close: close_a,
            own_closed: closed_b,
            peer_closed: closed_a,
        });
        (a, b)
    }
}

#[async_trait]
impl BridgeTransport for MemoryTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        use std::sync::atomic::Ordering;
        if self.own_closed.load(Ordering::SeqCst) || self.peer_closed.load(Ordering::SeqCst) {
            return Err(TransportError::ConnectionClosed(
                "MemoryTransport closed".into(),
            ));
        }
        self.out
            .send(frame)
            .await
            .map_err(|e| TransportError::ConnectionClosed(format!("send: {e}")))
    }

    async fn recv_frame(&self) -> Result<Value, TransportError> {
        use std::sync::atomic::Ordering;
        let mut inbox = self.inbox.lock().await;
        loop {
            // Fast paths: if EITHER side has already closed, drain
            // any buffered frame before declaring done. (Notify only
            // wakes on FUTURE notifications, so stale closes have to
            // be detected via the atomic flag.)
            if self.own_closed.load(Ordering::SeqCst) {
                return Err(TransportError::ConnectionClosed(
                    "MemoryTransport: own side closed".into(),
                ));
            }
            if self.peer_closed.load(Ordering::SeqCst) {
                if let Ok(v) = inbox.try_recv() {
                    return Ok(v);
                }
                return Err(TransportError::ConnectionClosed(
                    "MemoryTransport: peer closed".into(),
                ));
            }
            // Race the queue against the close notification so we
            // don't deadlock the read loop when EITHER side hangs up
            // while we're parked on `recv()`.
            tokio::select! {
                msg = inbox.recv() => {
                    match msg {
                        Some(v) => return Ok(v),
                        None => return Err(TransportError::ConnectionClosed(
                            "MemoryTransport: channel dropped".into(),
                        )),
                    }
                }
                _ = self.peer_close.notified() => {
                    // Loop back to drain any in-flight frame + the
                    // closed-state check.
                }
                _ = self.own_close.notified() => {
                    return Err(TransportError::ConnectionClosed(
                        "MemoryTransport: own side closed".into(),
                    ));
                }
            }
        }
    }

    async fn close(&self) {
        use std::sync::atomic::Ordering;
        if self.own_closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // `own_close` is shared with the peer as their `peer_close` —
        // one notify wakes anyone parked on either side.
        // (Including our own `recv_frame` if it's currently mid-select,
        // since that branch checks `own_closed` and bails out.)
        self.own_close.notify_waiters();
    }
}
