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
//! outbound  {type:"call",  id:corr, target, payload, caller, secret?}
//!                                     — `secret` = the item's marks
//!                                     (`fantastic_kernel::secret`)
//! outbound  {type:"watch", src} / {type:"unwatch", src}
//! inbound   {type:"reply", id, data}
//! inbound   {type:"error", id, error}
//! inbound   {type:"event", payload}  — re-emitted on the bridge's inbox
//! ```

use async_trait::async_trait;
use fantastic_kernel::Blob;
use serde_json::Value;
use std::fmt;

pub mod memory;
pub mod relay;
pub mod ws;

/// Every dialled socket (ws_bridge, relay_connector): a MESSAGE, like a frame,
/// is at most io_bridge's one frame cap (audit W-3). tungstenite's default
/// message cap is 64 MiB, so a peer's message fragmented into frames under the
/// frame cap was buffered and parsed at four times what python's client
/// (`websockets.connect(max_size=MAX_FRAME)`) allows.
pub(crate) fn ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(fantastic_io_bridge::codec::MAX_FRAME),
        max_frame_size: Some(fantastic_io_bridge::codec::MAX_FRAME),
        ..Default::default()
    }
}

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
    /// A JSON header + a raw body AT ITS SLOT: the header holds `null` where the
    /// body belongs, the [`Blob`]'s path is that slot's steps from the envelope
    /// root (the wire's `_binary_path`, so a python peer reinserts it there);
    /// rust uses the body alongside the header directly (its `Value` can't hold
    /// bytes). A decoded frame that named no slot is a [`Frame::Text`] — python
    /// places no body then.
    Binary(Value, Blob),
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

    /// Push a BINARY frame (`[4B len|header|body]` over a WS binary message):
    /// `header` holds `null` at the body's slot, `body.path` names that slot from
    /// the envelope root. Default: refuse — only stream-capable transports
    /// (memory/ws/relay) override it.
    async fn send_binary(&self, _header: Value, _body: Blob) -> Result<(), TransportError> {
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

    /// python `_BaseTransport.closed` — the engine's `connected`, `boot`'s
    /// idempotency and every forward/watch pre-check read it. Every transport
    /// answers it itself.
    fn closed(&self) -> bool;

    /// The ws seal: this transport refuses every call carrying secret marks (a
    /// `ws://` socket to a non-loopback host — python `WSTransport.sealed`). Only
    /// ws_bridge's `forward` reads it.
    fn seals_secrets(&self) -> bool {
        false
    }

    /// The relay leg's `is_live` — a usable socket exists RIGHT NOW (not closed,
    /// not mid-reconnect). Only `relay_connector`'s reflect reads it.
    fn is_live(&self) -> bool {
        !self.closed()
    }

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
