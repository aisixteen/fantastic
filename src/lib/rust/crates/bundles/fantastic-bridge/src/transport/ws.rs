//! WebSocket client transport — `tokio-tungstenite` against a remote
//! `fantastic-web` `/<peer_id>/ws` surface.
//!
//! Frames serialize as JSON text. The wire shape matches what
//! `fantastic-web`'s WS server expects (`{type:"call", target,
//! payload, id}` and `{type:"reply", id, data}` / `{type:"error", id,
//! error}`) so the same code path round-trips against any remote
//! kernel hosting a web agent.
//!
//! The connect helper does NOT spawn the read loop — the bridge
//! state owns that, since it needs the inbound frames routed to its
//! `kernel.send` dispatcher + pending oneshot map.
//!
//! PYTHON'S CLIENT, ON THE WIRE. `websockets.connect` offers
//! `permessage-deflate; client_max_window_bits` and deflates every message once
//! the server accepts; here the socket runs over a
//! [`fantastic_io_bridge::ws_deflate::DeflateStream`] that makes the same offer
//! and applies what the server's `101` settled. A send on a closed connection
//! fails with the text python's `ConnectionClosed` carries (`received 1001
//! (going away) peer going away; then sent 1001 (going away) peer going away`).
//!
//! A leg dialed to a NON-LOOPBACK host is SEALED for secrets ([`SECRET_SEALED`]).

use super::relay::sealed_call;
use super::{BridgeTransport, Frame, TransportError};
use async_trait::async_trait;
use fantastic_io_bridge::codec::{decode_binary_frame, encode_binary_frame};
use fantastic_io_bridge::ws_deflate::{DeflateStream, CLIENT_OFFER};
use fantastic_kernel::Blob;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{Error as WsError, ProtocolError, UrlError};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::stream::Mode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

type WsStream = WebSocketStream<DeflateStream<TcpStream>>;

/// The record `host` values that name THIS machine — a leg dialed to one of them
/// (an ssh tunnel's local end included) may carry a secret. Exact strings. Python
/// `_ws.LOOPBACK_HOSTS`.
pub const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

// THE WS SEAL (owner, 2026-10-03, audit S8). `ws://` carries every frame in clear,
// and a forward's secret values ride its frame verbatim (the far kernel needs
// them), so a leg dialed to a non-loopback host refuses any call carrying secret
// marks, in both directions: outbound in the bundle's `forward`
// (`crate::dispatch`), inbound in this transport's `recv_frame`. A loopback leg
// (an ssh tunnel's local end) is not sealed. One text for both.

/// The ws seal's refusal text — python `_ws.SECRET_SEALED`, byte for byte.
pub const SECRET_SEALED: &str = "ws_bridge: a call carrying secret marks may not cross a ws:// \
                                 leg to a non-loopback host — ws:// carries every frame in clear";

/// The ws seal's hint — python `_ws.SECRET_SEALED_HINT`.
pub const SECRET_SEALED_HINT: &str =
    "dial a loopback host (an ssh tunnel's local end) to send a secret";

/// The ws seal's refusal — outbound to the local caller, inbound as the reply to
/// the far caller. Python `_ws.secret_sealed()`.
#[must_use]
pub fn secret_sealed() -> Value {
    fantastic_kernel::refusal::refusal_hint(
        fantastic_kernel::refusal::UNAUTHORIZED,
        SECRET_SEALED,
        SECRET_SEALED_HINT,
    )
}

/// Whether a leg dialed to `host` is sealed — python `host not in LOOPBACK_HOSTS`.
#[must_use]
pub fn host_is_sealed(host: &str) -> bool {
    !LOOPBACK_HOSTS.contains(&host)
}

/// python `websockets.connect`'s `close_timeout`: how long `close()` waits for
/// the peer's close frame.
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// python `websockets.frames.Close.__str__`: `<code> (<explanation>)`, then the
/// reason when there is one.
fn close_text(code: u16, reason: &str) -> String {
    let explanation = match code {
        3000..=3999 => "registered",
        4000..=4999 => "private use",
        1000 => "OK",
        1001 => "going away",
        1002 => "protocol error",
        1003 => "unsupported data",
        1005 => "no status received [internal]",
        1006 => "abnormal closure [internal]",
        1007 => "invalid frame payload data",
        1008 => "policy violation",
        1009 => "message too big",
        1010 => "mandatory extension",
        1011 => "internal error",
        1012 => "service restart",
        1013 => "try again later",
        1014 => "bad gateway",
        1015 => "TLS handshake failure [internal]",
        _ => "unknown",
    };
    if reason.is_empty() {
        format!("{code} ({explanation})")
    } else {
        format!("{code} ({explanation}) {reason}")
    }
}

/// The closing handshake as this connection saw it — python's `close_rcvd`,
/// `close_sent` and `close_rcvd_then_sent` — and whether the socket has ended.
#[derive(Default)]
struct Closing {
    rcvd: Option<(u16, String)>,
    sent: Option<(u16, String)>,
    rcvd_then_sent: bool,
    /// The reader saw the socket's last frame (a close, an error, the end).
    ended: bool,
}

impl Closing {
    /// A close frame arrived; if none was sent, it is ECHOED (python echoes the
    /// frame's own code and reason, and so does tungstenite).
    fn received(&mut self, code: u16, reason: String) {
        if self.sent.is_none() {
            self.sent = Some((code, reason.clone()));
            self.rcvd_then_sent = true;
        }
        self.rcvd = Some((code, reason));
    }

    /// python `websockets`' `state in (CLOSING, CLOSED)`: a close frame was
    /// sent or received, or the socket is gone.
    fn is_closed(&self) -> bool {
        self.rcvd.is_some() || self.sent.is_some() || self.ended
    }

    /// python `str(ConnectionClosed(rcvd, sent, rcvd_then_sent))`.
    fn text(&self) -> String {
        let fmt = |c: &(u16, String)| close_text(c.0, &c.1);
        match (&self.rcvd, &self.sent) {
            (None, None) => "no close frame received or sent".to_string(),
            (None, Some(s)) => format!("sent {}; no close frame received", fmt(s)),
            (Some(r), None) => format!("received {}; no close frame sent", fmt(r)),
            (Some(r), Some(s)) if self.rcvd_then_sent => {
                format!("received {}; then sent {}", fmt(r), fmt(s))
            }
            (Some(r), Some(s)) => format!("sent {}; then received {}", fmt(s), fmt(r)),
        }
    }
}

/// tokio-tungstenite's `connect_async`, over a [`DeflateStream`] and with
/// python's offer on the request: the URL, the host, the TCP connect and the
/// handshake fail with the same error texts.
async fn connect_offering_deflate(url: &str) -> Result<WsStream, String> {
    let text = |e: WsError| e.to_string();
    let mut request = url.into_client_request().map_err(text)?;
    request.headers_mut().insert(
        "Sec-WebSocket-Extensions",
        HeaderValue::from_static(CLIENT_OFFER),
    );
    let domain = request
        .uri()
        .host()
        .ok_or_else(|| text(WsError::Url(UrlError::NoHostName)))?
        .to_string();
    let port = request
        .uri()
        .port_u16()
        .or_else(|| match request.uri().scheme_str() {
            Some("wss") => Some(443),
            Some("ws") => Some(80),
            _ => None,
        })
        .ok_or_else(|| text(WsError::Url(UrlError::UnsupportedUrlScheme)))?;
    let socket = TcpStream::connect(format!("{domain}:{port}"))
        .await
        .map_err(|e| text(WsError::Io(e)))?;
    // no TLS is built in: a `wss://` URL fails here, as `connect_async` does
    if let Mode::Tls =
        tokio_tungstenite::tungstenite::client::uri_mode(request.uri()).map_err(text)?
    {
        return Err(text(WsError::Url(UrlError::TlsFeatureNotEnabled)));
    }
    let (stream, _resp) = tokio_tungstenite::client_async_with_config(
        request,
        DeflateStream::client(socket),
        Some(super::ws_config()),
    )
    .await
    .map_err(text)?;
    Ok(stream)
}

/// python `logger.warning("ws_bridge: skipping malformed frame: %s", e)`
/// (`ws_bridge/_ws.py:78`). python configures no logging handler, so the record
/// reaches `logging.lastResort` and the BARE message lands on stderr. The `<e>`
/// tail is the codec's own text (residual R-D4-8 for the library half).
fn skip_malformed(err: &str) {
    eprintln!("ws_bridge: skipping malformed frame: {err}");
}

/// python `logger.warning("ws_bridge: skipping non-object frame")`
/// (`ws_bridge/_ws.py:81`) — a frame that decoded, but not to a dict.
fn skip_non_object() {
    eprintln!("ws_bridge: skipping non-object frame");
}

/// WebSocket client transport.
///
/// On construction we split the socket and spawn a reader task that
/// pushes inbound text frames onto an mpsc queue. `recv_frame`
/// reads from that queue; `send_frame` writes directly to the
/// sink (guarded by a Mutex so concurrent sends serialize cleanly).
pub struct WsTransport {
    sink: Mutex<futures_util::stream::SplitSink<WsStream, Message>>,
    rx: Mutex<mpsc::Receiver<Result<Frame, TransportError>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    closing: Arc<std::sync::Mutex<Closing>>,
    /// Dialed to a non-loopback host — the ws seal holds ([`SECRET_SEALED`]).
    sealed: bool,
}

impl WsTransport {
    /// Connect to `url` (e.g. `ws://host:port/peer_id/ws`) with a
    /// default 5s timeout and return a transport wrapping the live
    /// socket. Spawns the reader task as a side-effect. `sealed`: the
    /// host is not loopback ([`host_is_sealed`]).
    pub async fn connect(url: &str, sealed: bool) -> Result<Arc<Self>, TransportError> {
        Self::connect_with_timeout(url, std::time::Duration::from_secs(5), sealed).await
    }

    /// Connect with an explicit timeout. Used by tests + callers who
    /// want a tighter bound than the 5s default.
    pub async fn connect_with_timeout(
        url: &str,
        timeout: std::time::Duration,
        sealed: bool,
    ) -> Result<Arc<Self>, TransportError> {
        let connect_fut = connect_offering_deflate(url);
        let stream = match tokio::time::timeout(timeout, connect_fut).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(TransportError::Other(format!("ws connect {url}: {e}"))),
            Err(_) => {
                return Err(TransportError::Other(format!(
                    "ws connect {url}: timeout after {:?}",
                    timeout
                )))
            }
        };
        let (sink, mut source) = stream.split();
        let (tx, rx) = mpsc::channel::<Result<Frame, TransportError>>(64);
        let closing: Arc<std::sync::Mutex<Closing>> = Arc::default();
        let seen = Arc::clone(&closing);
        let reader = tokio::spawn(async move {
            while let Some(msg) = source.next().await {
                let frame_result = match msg {
                    // Text WS message → a plain JSON envelope. A frame that does
                    // not decode, or decodes to a non-object, is SKIPPED with
                    // python's line and the reader keeps going (A15.1) — the peer
                    // is remote, so bad bytes are an input, not a bug. Raising
                    // here killed the engine read loop while `closed` stayed
                    // false, leaving a bridge that reflects `connected:true` and
                    // answers every forward with a timeout that never names the
                    // dead reader.
                    // STRICT (D13): python `decode_frame` → `io_bridge.loads`.
                    Ok(Message::Text(t)) => match fantastic_io_bridge::codec::loads(&t) {
                        Ok(v) if v.is_object() => Ok(Frame::Text(v)),
                        Ok(_) => {
                            skip_non_object();
                            continue;
                        }
                        Err(e) => {
                            skip_malformed(&format!("bad text frame: {e}"));
                            continue;
                        }
                    },
                    // Binary WS message → the `[4B len|header|body]` codec frame
                    // (a read_stream/write_stream chunk — raw bytes, no base64).
                    // A frame that named no slot places no body — python's
                    // decode returns the plain envelope.
                    Ok(Message::Binary(b)) => match decode_binary_frame(&b) {
                        Ok(f) if f.head.is_object() => Ok(match f.path {
                            Some(path) => Frame::Binary(f.head, Blob::new(path, f.body)),
                            None => Frame::Text(f.head),
                        }),
                        Ok(_) => {
                            skip_non_object();
                            continue;
                        }
                        Err(e) => {
                            skip_malformed(&e);
                            continue;
                        }
                    },
                    Ok(Message::Close(frame)) => {
                        // python's `Close.parse(b"")` for an empty close frame
                        let (code, reason) = frame.map_or((1005, String::new()), |f| {
                            (u16::from(f.code), f.reason.into_owned())
                        });
                        seen.lock()
                            .expect("closing poisoned")
                            .received(code, reason);
                        let _ = tx
                            .send(Err(TransportError::ConnectionClosed(
                                "peer sent close frame".into(),
                            )))
                            .await;
                        // KEEP READING until the socket ends: tungstenite sends
                        // the queued echo of a close on its next read (python
                        // echoes at once), and the peer then drops the socket.
                        while let Some(Ok(_)) = source.next().await {}
                        break;
                    }
                    Ok(_) => continue, // ping/pong/etc handled by tungstenite
                    Err(e) => Err(TransportError::ConnectionClosed(format!("ws read: {e}"))),
                };
                let is_err = matches!(&frame_result, Err(TransportError::ConnectionClosed(_)));
                if tx.send(frame_result).await.is_err() {
                    break;
                }
                if is_err {
                    break;
                }
            }
            seen.lock().expect("closing poisoned").ended = true;
        });
        Ok(Arc::new(Self {
            sink: Mutex::new(sink),
            rx: Mutex::new(rx),
            reader_task: Mutex::new(Some(reader)),
            closing,
            sealed,
        }))
    }

    /// A failed send, as python reports it: on a closed (or broken)
    /// connection, `ConnectionClosed`'s text — what was received and sent in
    /// the closing handshake; anything else, the library's own text.
    fn send_error(&self, e: &WsError) -> TransportError {
        let closed = matches!(
            e,
            WsError::ConnectionClosed
                | WsError::AlreadyClosed
                | WsError::Io(_)
                | WsError::Protocol(
                    ProtocolError::SendAfterClosing | ProtocolError::ResetWithoutClosingHandshake
                )
        );
        let text = if closed {
            self.closing.lock().expect("closing poisoned").text()
        } else {
            e.to_string()
        };
        TransportError::ConnectionClosed(text)
    }
}

#[async_trait]
impl BridgeTransport for WsTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        // python's `encode_frame` text half (A13.14).
        let text = fantastic_io_bridge::codec::encode_frame(&frame);
        let mut sink = self.sink.lock().await;
        sink.send(Message::Text(text))
            .await
            // python `ConnectionClosed(str(e))` — the library's text, no prefix.
            .map_err(|e| self.send_error(&e))
    }

    async fn send_binary(&self, header: Value, body: Blob) -> Result<(), TransportError> {
        let wire = encode_binary_frame(&header, &body.path, &body.bytes);
        let mut sink = self.sink.lock().await;
        sink.send(Message::Binary(wire))
            .await
            .map_err(|e| self.send_error(&e))
    }

    /// python `WSTransport.closed` — the socket's own state, so a leg whose
    /// remote dropped reflects `connected:false` and `boot` dials again.
    fn closed(&self) -> bool {
        self.closing.lock().expect("closing poisoned").is_closed()
    }

    async fn recv_frame(&self) -> Result<Frame, TransportError> {
        let mut rx = self.rx.lock().await;
        loop {
            let frame = match rx.recv().await {
                Some(r) => r?,
                None => {
                    return Err(TransportError::ConnectionClosed(
                        "ws reader task ended".into(),
                    ))
                }
            };
            let head = match &frame {
                Frame::Text(v) | Frame::Binary(v, _) => v,
            };
            if self.sealed {
                if let Some(id) = sealed_call(head) {
                    // THE WS SEAL, inbound: the frame already crossed the network
                    // in clear, and nothing here dispatches it — the far caller is
                    // answered with the seal's refusal.
                    // TODO(ws seal): wss:// (TLS) is the later path — a TLS leg
                    // carries secrets to any host, and this seal lifts for it.
                    let reply = json!({"type": "reply", "id": id, "data": secret_sealed()});
                    // the socket dropped → the next read reports it
                    let _ = self.send_frame(reply).await;
                    continue;
                }
            }
            return Ok(frame);
        }
    }

    fn seals_secrets(&self) -> bool {
        self.sealed
    }

    async fn close(&self) {
        // Tear down sink first so any in-flight send fails fast. python
        // `ws.close()` closes with 1000 and no reason — unless the peer's close
        // already arrived, which it has echoed.
        {
            let mut sink = self.sink.lock().await;
            let first = {
                let mut c = self.closing.lock().expect("closing poisoned");
                let first = c.sent.is_none();
                if first {
                    c.sent = Some((1000, String::new()));
                }
                first
            };
            if first {
                let _ = sink
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: "".into(),
                    })))
                    .await;
            }
            let _ = sink.close().await;
        }
        // python's `close()` WAITS for the peer's close frame (its
        // `close_timeout`, 10 s) before it lets the connection go — so a send
        // after it reports `sent 1000 (OK); then received 1000 (OK)`.
        if let Some(handle) = self.reader_task.lock().await.take() {
            let abort = handle.abort_handle();
            if tokio::time::timeout(CLOSE_TIMEOUT, handle).await.is_err() {
                abort.abort();
            }
        }
    }
}
