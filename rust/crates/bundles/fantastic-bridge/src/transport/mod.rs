//! Transport abstraction for the kernel_bridge.
//!
//! Every transport exposes the same triple — `send_frame`,
//! `recv_frame`, `close` — so the bridge's read-loop is one piece
//! of code regardless of how bytes move between kernels:
//!
//! - [`memory`] — in-process pair, used for unit tests + the
//!   `inject_pair` test seam. No I/O, deterministic.
//! - [`ws`] — `tokio-tungstenite` client against a remote
//!   `fantastic-web` WebSocket surface.
//!
//! Frame shape on the wire (asymmetric — bridge is a pure client;
//! mirrors the Python kernel_bridge):
//!
//! ```text
//! outbound  {type:"call",  id:corr, target, payload}
//! outbound  {type:"watch", src} / {type:"unwatch", src}
//! inbound   {type:"reply", id, data}
//! inbound   {type:"error", id, error}
//! inbound   {type:"event", payload}  — re-emitted on the bridge's inbox
//! ```

use async_trait::async_trait;
use serde_json::Value;
use std::fmt;

pub mod memory;
pub mod relay;
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

/// One frame on the wire. A **text** frame is plain JSON (the call/reply/event
/// envelopes); a **binary** frame carries a JSON header PLUS a raw body (a
/// `read_stream`/`write_stream` chunk) — raw bytes, never base64. The
/// text/binary split is carried by the TRANSPORT (the WS frame type — every
/// transport is WS-based; the relay forwards the frame kind end-to-end),
/// matching the shared `io_bridge` codec.
#[derive(Debug)]
pub enum Frame {
    /// A JSON envelope with no raw bytes.
    Text(Value),
    /// A JSON header + a raw body. The header carries `_binary_path` naming
    /// where the body belongs (so a python peer reinserts it); rust uses the
    /// body alongside the header directly (its `Value` can't hold bytes).
    Binary(Value, Vec<u8>),
}

impl Frame {
    /// The JSON envelope (a text frame's value, or a binary frame's header).
    /// Convenience for callers/tests that only need the envelope fields.
    pub fn into_value(self) -> Value {
        match self {
            Frame::Text(v) => v,
            Frame::Binary(h, _) => h,
        }
    }
}

/// Unified send/recv/close surface every transport implements.
///
/// Implementations are object-safe (`Arc<dyn BridgeTransport>`) so
/// the per-agent state can carry one without monomorphizing the
/// dispatch path on transport kind.
#[async_trait]
pub trait BridgeTransport: Send + Sync {
    /// Push a TEXT frame to the peer. Returns when the bytes are
    /// queued (memory) / sent over the socket (ws).
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError>;

    /// Push a BINARY frame (`[4B len|header|body]` over a WS binary message).
    /// Default: refuse — only stream-capable transports (memory/ws/ssh/relay)
    /// override it.
    async fn send_binary(&self, _header: Value, _body: Vec<u8>) -> Result<(), TransportError> {
        Err(TransportError::Other(
            "transport does not carry binary frames".into(),
        ))
    }

    /// Pull the next inbound frame (text or binary). Blocks until one arrives.
    /// Returns [`TransportError::ConnectionClosed`] on peer hang-up — the
    /// bridge's read-loop watches for this as the signal to wind down.
    async fn recv_frame(&self) -> Result<Frame, TransportError>;

    /// Idempotent close. Subsequent `recv_frame` resolves with
    /// `ConnectionClosed`; subsequent `send_frame` likewise.
    async fn close(&self);

    /// Directory snapshot from a relay-kernel router (`relay_connector` only):
    /// `{peers:[{guid,status,last_seen,since}]}`. Default: not a relay transport.
    async fn list_peers(&self, _timeout_s: f64) -> Value {
        serde_json::json!({"error": "transport has no relay directory"})
    }
    /// Subscribe to the relay directory; `peer_*` events re-emit on the connector
    /// inbox. Default: not a relay transport.
    async fn watch_directory(&self, _timeout_s: f64) -> Value {
        serde_json::json!({"error": "transport has no relay directory"})
    }
    /// Stop the directory subscription. Default: no-op.
    async fn unwatch_directory(&self) -> Value {
        serde_json::json!({"ok": true})
    }
    /// Advertise/replace this peer's directory attributes (the opaque blob the relay
    /// reflects into `list_peers` + a `peer_updated` event). Default: not a relay
    /// transport.
    async fn set_identity(&self, _attrs: Value) -> Value {
        serde_json::json!({"error": "transport has no relay directory"})
    }
}
