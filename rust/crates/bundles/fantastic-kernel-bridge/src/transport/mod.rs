//! Transport abstraction for the kernel_bridge.
//!
//! Every transport exposes the same triple — `send_frame`,
//! `recv_frame`, `close` — so the bridge's read-loop is one piece
//! of code regardless of how bytes move between kernels. Three
//! implementations ship today:
//!
//! - [`memory`] — in-process pair, used for unit tests + the
//!   `inject_pair` test seam. No I/O, deterministic.
//! - [`ws`] — `tokio-tungstenite` client against a remote
//!   `fantastic-web` WebSocket surface.
//! - [`http`] — `reqwest` POST against a remote `web_rest`
//!   surface. Asymmetric (no server push); `recv_frame` reads off
//!   a self-populated queue that `send_frame` fills with the HTTP
//!   response synchronously.
//!
//! Frame shape on the wire (mirrors Python's kernel_bridge):
//!
//! ```text
//! outbound  {type:"call",  target:peer_id,
//!            payload:{type:"forward", target, payload, corr_id},
//!            id:corr_id}
//! inbound   {type:"call",  ...}  — peer-originated, read loop dispatches via kernel.send
//! inbound   {type:"reply", id, data}
//! inbound   {type:"error", id, error}
//! ```

use async_trait::async_trait;
use serde_json::Value;
use std::fmt;

pub mod http;
pub mod memory;
#[cfg(feature = "full")]
pub mod ssh;
pub mod ws;

/// Errors a transport can raise. Kept narrow on purpose — every
/// failure is either "the peer hung up" or "I couldn't serialize
/// the frame", and both surface as the same flavour of pending-
/// future rejection upstream.
#[derive(Debug)]
pub enum TransportError {
    /// Peer closed the channel (clean or abrupt). Bridge read-loop
    /// treats this as a terminal signal — emits `bridge_down`,
    /// fails every pending forward.
    ConnectionClosed(String),
    /// Serialization / network error that isn't a clean close.
    /// Treated equivalently — the bridge can't distinguish, and
    /// the caller's reply is already lost either way.
    Other(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionClosed(m) => write!(f, "ConnectionClosed: {m}"),
            Self::Other(m) => write!(f, "transport error: {m}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Unified send/recv/close surface every transport implements.
///
/// Implementations are object-safe (`Arc<dyn BridgeTransport>`) so
/// the per-agent state can carry one without monomorphizing the
/// dispatch path on transport kind.
#[async_trait]
pub trait BridgeTransport: Send + Sync {
    /// Push a frame to the peer. Returns when the bytes are
    /// queued (memory) / sent over the socket (ws) / the HTTP
    /// round-trip is complete (http).
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError>;

    /// Pull the next inbound frame. Blocks until one arrives.
    /// Returns [`TransportError::ConnectionClosed`] on peer
    /// hang-up — the bridge's read-loop watches for this as the
    /// signal to wind down.
    async fn recv_frame(&self) -> Result<Value, TransportError>;

    /// Idempotent close. Subsequent `recv_frame` resolves with
    /// `ConnectionClosed`; subsequent `send_frame` likewise.
    async fn close(&self);
}
