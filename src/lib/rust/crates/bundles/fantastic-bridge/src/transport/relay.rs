//! Relay-kernel router transport — `tokio-tungstenite` to `../fantastic_relay`.
//!
//! Mirrors `python/bundled_agents/io/relay_connector/_relay.py::RelayTransport`.
//! The relay is itself a kernel: we dial `ws://<host>/<guid>` (subprotocol
//! `fantastic.relay.v1`, header `X-Fantastic-Auth: <group password>` checked once
//! at the WS upgrade) and reach a fixed `partner` GUID. The password is a SECRET
//! and lives only in a credential_store (audit IO-7/F4, 2026-10-03): the
//! transport is handed a [`PasswordReader`] and awaits it before EVERY dial —
//! nothing cached, a rotated password is used by the next dial. The relay routes by
//! `target` and delivers peer→peer ONE-WAY as `{type:"event", source, payload}`.
//!
//! So this transport TUNNELS the shared bridge frames: `send` wraps each frame in
//! a relay envelope `{type:"send", target:<partner>, payload:<frame>}`; `recv`
//! accepts only `{type:"event", source==partner}` and returns the inner frame.
//! PURE STREAMS via the shared codec: a control frame is a TEXT WS frame, a
//! `read_stream` chunk a BINARY WS frame `[4B len|header|body]` — raw bytes, NO
//! base64. For a binary frame the `_binary_path` is lifted one step, under the
//! envelope's `payload` (`["data", "bytes"]` → `["payload", "data", "bytes"]`).
//!
//! Resilience: the connection is SELF-HEALING. A supervisor task owns the socket
//! — on a drop it re-dials after `reconnect` seconds (default 10; `0` = one-shot)
//! and swaps in the new sink, transparently to the engine's read loop. The
//! initial dial is eager; if it fails with reconnect on, the leg still boots and
//! connects in the background. A keepalive task sends the relay's `keepalive`
//! verb to stay `green`.

use super::{BridgeTransport, Frame, TransportError};
use async_trait::async_trait;
use fantastic_io_bridge::codec::{decode_binary_frame, encode_binary_frame, BinaryFrame};
use fantastic_kernel::Blob;
use fantastic_kernel::{AgentId, Kernel};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
/// The one live writer, behind a lock. Generic in the sink so the bounded-write
/// path can be driven by a test sink that never completes — the stalled-peer case
/// that used to hold this lock forever.
type SharedSinkOf<S> = Arc<Mutex<Option<S>>>;
type SharedSink = SharedSinkOf<WsSink>;
/// The directory attributes we advertise to the relay (an opaque blob — the relay
/// stores + reflects it, never interprets it). Shared + mutable so `set_identity`
/// updates it and every reconnect re-announces the LATEST value. `Null`/`{}` ⇒ a
/// plain peer, no announce sent.
type SharedIdentity = Arc<std::sync::Mutex<Value>>;
/// Outstanding relay-LEVEL requests (the directory `call`/`watch target:relay`),
/// keyed by minted id. Distinct from the engine's partner bridge-frame correlation.
type RelayPending = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Value>>>>;

/// The WS subprotocol the relay pairs + authenticates on.
pub const SUBPROTOCOL: &str = "fantastic.relay.v1";

/// The relay's own directory agent: a directory request is addressed `target` to
/// it, and a directory event reaches the engine as an `event` frame from `src` it
/// — so `watch_directory` subscribes the leg to it like any watched agent (python
/// `_relay.DIRECTORY_SRC`).
pub const DIRECTORY_SRC: &str = "relay";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The write DEADLINE on the relay sink.
///
/// Every write took the sink `Mutex` and then awaited `s.send(...)` with no bound.
/// When the TCP send buffer to the relay fills (the host stopped reading — a
/// partition, a wedged process), that await never returns and the lock is held
/// forever: every other forward, the keepalive, AND `close()` park behind it, so
/// the bridge agent can never be torn down. A deadline turns an unbounded stall
/// into one refusal plus a re-dial.
#[cfg(not(test))]
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Short under test so the stalled-peer case is exercised in milliseconds.
#[cfg(test)]
const SEND_TIMEOUT: Duration = Duration::from_millis(300);

/// Write one message to the live sink under [`SEND_TIMEOUT`].
///
/// On expiry the sink is DROPPED (the lock is released for good, nothing else
/// parks behind the dead write) and the leg is marked disconnected, so the
/// supervisor re-dials instead of the whole transport wedging.
async fn send_bounded<S>(
    sink: &SharedSinkOf<S>,
    connected: &AtomicBool,
    msg: Message,
) -> Result<(), SendFail>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let mut guard = sink.lock().await;
    let Some(s) = guard.as_mut() else {
        return Err(SendFail::NotConnected);
    };
    match tokio::time::timeout(SEND_TIMEOUT, s.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        // python's `str(e)` — the library's text, with no prefix of ours.
        Ok(Err(e)) => Err(SendFail::Failed(e.to_string())),
        Err(_elapsed) => {
            guard.take();
            connected.store(false, Ordering::SeqCst);
            Err(SendFail::Failed(format!(
                "timed out after {SEND_TIMEOUT:?} (peer not reading)"
            )))
        }
    }
}

/// Why a bounded write did not land. The directory surface answers
/// `not_connected` and `transport_error` as DIFFERENT codes, so "no socket" and
/// "the socket refused/stalled" must not collapse into one string.
enum SendFail {
    /// There is no live sink right now (pre-boot, or mid-reconnect).
    NotConnected,
    /// A socket existed and the write failed or outran [`SEND_TIMEOUT`].
    Failed(String),
}

impl From<SendFail> for TransportError {
    fn from(f: SendFail) -> Self {
        match f {
            // python `ConnectionClosed("relay_connector: not connected")`.
            SendFail::NotConnected => {
                Self::ConnectionClosed("relay_connector: not connected".into())
            }
            SendFail::Failed(m) => Self::ConnectionClosed(m),
        }
    }
}

/// python's `spawn=lambda name, coro: kernel.ctx.spawn_task(rec["id"], name, coro)`
/// — the heartbeat is the connector's OWN registered work, so it has a `ps` row
/// and a `cancel` that reaches it.
#[derive(Clone)]
pub struct RelaySpawn {
    kernel: Arc<Kernel>,
    agent_id: AgentId,
}

impl RelaySpawn {
    /// Bind the seam to one connector agent.
    #[must_use]
    pub fn new(kernel: &Arc<Kernel>, agent_id: &AgentId) -> Self {
        Self {
            kernel: Arc::clone(kernel),
            agent_id: agent_id.clone(),
        }
    }

    fn spawn<F>(&self, verb: &str, work: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.kernel.spawn_task(&self.agent_id, verb, work);
    }
}

/// Relay router transport with a self-healing connection. `send` writes to the
/// current sink (errors while mid-reconnect); `recv` pulls frames the supervisor
/// pumps across reconnects.
pub struct RelayTransport {
    partner: String,
    sink: SharedSink,
    rx: Mutex<mpsc::Receiver<Result<Frame, TransportError>>>,
    closed: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    /// Fired by `close` so the registered heartbeat task ends at once (python
    /// cancels its `_hb_task`).
    stop_heartbeat: Arc<tokio::sync::Notify>,
    relay_pending: RelayPending,
    relay_next_id: AtomicU64,
    identity: SharedIdentity,
}

/// The relay password, read at dial time — python `PasswordReader`, `() -> str`:
/// `Err` names the store and the ref (python's `CredentialError`).
pub type PasswordReader =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Result<String, String>> + Send + Sync>;

/// One dial: the password read at USE time, every (re)dial — a rotated password
/// reaches the relay on the next connect, and no copy outlives this call. Python
/// `_default_dialer`'s `dial`.
async fn dial(url: &str, password: &PasswordReader) -> Result<WsStream, TransportError> {
    let token = password().await.map_err(TransportError::Other)?;
    open_ws(url, &token).await
}

/// THE network dial — the one place a relay socket opens (python `_open_ws`):
/// the WS upgrade request (path GUID + subprotocol + auth header).
async fn open_ws(url: &str, token: &str) -> Result<WsStream, TransportError> {
    let mut req = url
        .into_client_request()
        .map_err(|e| TransportError::Other(format!("relay url {url}: {e}")))?;
    {
        let h = req.headers_mut();
        h.append(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(SUBPROTOCOL),
        );
        h.append(
            "X-Fantastic-Auth",
            HeaderValue::from_str(token)
                .map_err(|e| TransportError::Other(format!("relay auth header: {e}")))?,
        );
    }
    let dial = connect_async_with_config(req, Some(super::ws_config()), false);
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, dial).await {
        Ok(Ok((stream, _resp))) => Ok(stream),
        Ok(Err(e)) => Err(TransportError::Other(format!("relay connect {url}: {e}"))),
        Err(_) => Err(TransportError::Other(format!(
            "relay connect {url}: timeout"
        ))),
    }
}

impl RelayTransport {
    /// Dial the relay as `guid` and tunnel to `partner_guid`. `reconnect_secs` is
    /// the backoff before each re-dial (0 = one-shot: the initial dial must
    /// succeed, and a later drop is terminal). The initial dial is eager.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        relay_url: &str,
        guid: &str,
        password: PasswordReader,
        partner_guid: &str,
        reconnect_secs: f64,
        heartbeat_secs: f64,
        identity: Value,
        spawn: RelaySpawn,
    ) -> Result<Arc<Self>, TransportError> {
        let url = format!("{}/{}", relay_url.trim_end_matches('/'), guid);
        let partner = partner_guid.to_string();
        let sink: SharedSink = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel::<Result<Frame, TransportError>>(64);
        let closed = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let relay_pending: RelayPending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let identity: SharedIdentity = Arc::new(std::sync::Mutex::new(identity));
        let stop = Arc::new(tokio::sync::Notify::new());

        // Eager first dial. On success seed the initial source; on failure with
        // reconnect off, fail the boot; otherwise heal in the background.
        let initial: Option<SplitStream<WsStream>> = match dial(&url, &password).await {
            Ok(stream) => {
                let (s, source) = stream.split();
                *sink.lock().await = Some(s);
                connected.store(true, Ordering::SeqCst);
                Some(source)
            }
            Err(e) => {
                if reconnect_secs <= 0.0 {
                    return Err(e);
                }
                None
            }
        };
        // python's `_on_socket_open`: the beat, then the announce — BOTH only
        // where a socket came up. Each later reconnect repeats the pair (in
        // supervise).
        if initial.is_some() {
            start_heartbeat(&spawn, &sink, &closed, &connected, &stop, heartbeat_secs);
        }
        send_announce(&sink, &connected, &identity).await;

        // THE CONNECTOR'S OWN WORK: `connect` runs in its `boot` dispatch, so the
        // supervisor carries that identity (`spawn_inherit`) — every re-dial's
        // password read is the connector's send, gated as it.
        let supervisor = fantastic_kernel::spawn_inherit(supervise(
            url,
            password,
            partner.clone(),
            Arc::clone(&sink),
            tx,
            Arc::clone(&closed),
            Arc::clone(&connected),
            Arc::clone(&relay_pending),
            Arc::clone(&identity),
            reconnect_secs,
            initial,
            spawn.clone(),
            Arc::clone(&stop),
            heartbeat_secs,
        ));

        Ok(Arc::new(Self {
            partner,
            sink,
            rx: Mutex::new(rx),
            closed,
            connected,
            supervisor: Mutex::new(Some(supervisor)),
            stop_heartbeat: Arc::clone(&stop),
            relay_pending,
            relay_next_id: AtomicU64::new(0),
            identity,
        }))
    }

    /// Send a relay-LEVEL frame to the directory (`target:"relay"` + a minted id)
    /// and await the correlated `{type:"reply", id, data}`. Bypasses the partner
    /// tunnel. Returns an error object if disconnected / timed out.
    async fn relay_request(&self, mut frame: Value, timeout_s: f64) -> Value {
        // python: no socket ⇒ `not_connected` BEFORE an id is spent; then
        // `self._relay_next_id += 1; rid = f"dir_{n}"` — the first id is `dir_1`.
        if self.sink.lock().await.is_none() {
            return json!({"error":"relay_connector: not connected","reason":"not_connected"});
        }
        let id = format!(
            "dir_{}",
            self.relay_next_id.fetch_add(1, Ordering::SeqCst) + 1
        );
        frame["id"] = json!(id);
        frame["target"] = json!("relay");
        let (tx, rx) = oneshot::channel::<Value>();
        self.relay_pending
            .lock()
            .expect("relay_pending poisoned")
            .insert(id.clone(), tx);
        if let Err(fail) = send_bounded(
            &self.sink,
            &self.connected,
            Message::Text(fantastic_io_bridge::codec::encode_frame(&frame)),
        )
        .await
        {
            self.relay_pending.lock().unwrap().remove(&id);
            return match fail {
                SendFail::NotConnected => {
                    json!({"error":"relay_connector: not connected","reason":"not_connected"})
                }
                SendFail::Failed(m) => {
                    json!({"error": format!("relay_connector: directory send failed: {m}"),
                           "reason":"transport_error"})
                }
            };
        }
        match tokio::time::timeout(Duration::from_secs_f64(timeout_s), rx).await {
            Ok(Ok(v)) => v,
            _ => {
                self.relay_pending.lock().unwrap().remove(&id);
                json!({
                    "error": format!(
                        "relay_connector: directory timeout after {}s",
                        fantastic_bundle::py_float_repr(timeout_s)
                    ),
                    "reason": "timeout"
                })
            }
        }
    }

    /// A usable socket exists right now.
    pub fn is_live(&self) -> bool {
        !self.closed.load(Ordering::SeqCst) && self.connected.load(Ordering::SeqCst)
    }
}

/// Pump one WS source until it ends. Classifies each frame: a relay-level
/// `reply` resolves a pending directory request; a `source:"relay"` event is
/// surfaced as a bridge `event` (the engine re-emits it on the connector inbox);
/// a `source:partner` delivery is unwrapped to the inner bridge frame.
async fn pump(
    mut source: SplitStream<WsStream>,
    partner: &str,
    relay_pending: &RelayPending,
    tx: &mpsc::Sender<Result<Frame, TransportError>>,
) {
    while let Some(msg) = source.next().await {
        let frame = match msg {
            // STRICT (D13): python `decode_frame` → `io_bridge.loads`.
            Ok(Message::Text(t)) => match fantastic_io_bridge::codec::loads(&t) {
                Ok(env) => classify_text(env, partner, relay_pending),
                Err(_) => None,
            },
            // Binary frames are partner stream chunks only (directory events are text).
            Ok(Message::Binary(b)) => match decode_binary_frame(&b) {
                Ok(f) => unwrap_binary(f, partner),
                Err(_) => None,
            },
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue, // ping/pong handled by tungstenite
        };
        if let Some(f) = frame {
            if tx.send(Ok(f)).await.is_err() {
                return;
            }
        }
    }
}

/// Tunnel one bridge frame to `partner`: the relay envelope `{type:"send",
/// target:<partner>, payload:<frame>}` as a TEXT frame — python
/// `RelayTransport.send`.
async fn send_to_partner(
    sink: &SharedSink,
    connected: &AtomicBool,
    partner: &str,
    frame: Value,
) -> Result<(), TransportError> {
    let env = json!({"type":"send","target":partner,"payload":frame});
    // python's `encode_frame` text half (A13.14).
    let text = fantastic_io_bridge::codec::encode_frame(&env);
    send_bounded(sink, connected, Message::Text(text))
        .await
        .map_err(TransportError::from)
}

/// Classify a text relay frame. Resolves a directory `reply` (side effect, no
/// frame); surfaces a `source:"relay"` event as `{type:"event", src:"relay",
/// payload}`; unwraps a `source:partner` delivery. Returns None for anything else.
fn classify_text(env: Value, partner: &str, relay_pending: &RelayPending) -> Option<Frame> {
    match env.get("type").and_then(Value::as_str) {
        Some("reply") => {
            if let Some(id) = env.get("id").and_then(Value::as_str) {
                if let Some(tx) = relay_pending
                    .lock()
                    .expect("relay_pending poisoned")
                    .remove(id)
                {
                    // python `msg.get("data") or {}`.
                    let data = match env.get("data") {
                        Some(v) if fantastic_bundle::py_truthy(v) => v.clone(),
                        _ => json!({}),
                    };
                    let _ = tx.send(data);
                }
            }
            None
        }
        Some("event") => match env.get("source").and_then(Value::as_str) {
            // python skips ANY event whose payload is not a dict, directory
            // included, before it ever looks at the source.
            _ if !env.get("payload").is_some_and(Value::is_object) => None,
            // Directory event → an `event` from the directory, which the engine
            // re-emits on the connector inbox while `watch_directory` holds it in
            // the leg's `watching`.
            Some(DIRECTORY_SRC) => Some(Frame::Text(json!({
                "type": "event",
                "src": DIRECTORY_SRC,
                "payload": env.get("payload").cloned().unwrap_or(Value::Null),
            }))),
            // Partner tunnel delivery → the inner bridge frame.
            Some(s) if s == partner => Some(Frame::Text(
                env.get("payload").cloned().unwrap_or(Value::Null),
            )),
            _ => None,
        },
        _ => None,
    }
}

/// The connection supervisor: pump the current source; on a drop, re-dial with
/// the backoff and swap in the new sink. Terminal on explicit close (or one-shot
/// drop) — pushes ConnectionClosed so `recv_frame` (the engine read loop) ends.
#[allow(clippy::too_many_arguments)]
async fn supervise(
    url: String,
    password: PasswordReader,
    partner: String,
    sink: SharedSink,
    tx: mpsc::Sender<Result<Frame, TransportError>>,
    closed: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    relay_pending: RelayPending,
    identity: SharedIdentity,
    reconnect_secs: f64,
    mut initial: Option<SplitStream<WsStream>>,
    spawn: RelaySpawn,
    stop: Arc<tokio::sync::Notify>,
    heartbeat_secs: f64,
) {
    let backoff = Duration::from_secs_f64(reconnect_secs.max(0.0));
    // A FAILED EAGER DIAL re-dials AT ONCE: python's `recv()` finds `_ws` None
    // and `_reconnect_ws` dials before any sleep — the backoff only follows a
    // failure or a drop.
    let mut dial_now = initial.is_none();
    loop {
        if closed.load(Ordering::SeqCst) {
            break;
        }
        let source = if let Some(s) = initial.take() {
            s // first iteration: the eager connection (announced in connect())
        } else {
            if !std::mem::take(&mut dial_now) {
                tokio::time::sleep(backoff).await;
            }
            if closed.load(Ordering::SeqCst) {
                break;
            }
            match dial(&url, &password).await {
                Ok(stream) => {
                    let (s, source) = stream.split();
                    *sink.lock().await = Some(s);
                    connected.store(true, Ordering::SeqCst);
                    // python `_on_socket_open` again: re-beat, then re-advertise
                    // (the relay drops our state on disconnect).
                    start_heartbeat(&spawn, &sink, &closed, &connected, &stop, heartbeat_secs);
                    send_announce(&sink, &connected, &identity).await;
                    source
                }
                Err(_) => {
                    if reconnect_secs <= 0.0 {
                        break;
                    }
                    continue;
                }
            }
        };
        pump(source, &partner, &relay_pending, &tx).await;
        connected.store(false, Ordering::SeqCst);
        *sink.lock().await = None;
        if reconnect_secs <= 0.0 {
            break; // one-shot: a drop is terminal.
        }
    }
    connected.store(false, Ordering::SeqCst);
    *sink.lock().await = None;
    let _ = tx
        .send(Err(TransportError::ConnectionClosed("relay closed".into())))
        .await;
}

/// Advertise our directory attributes to the relay (`{type:"announce", attrs,
/// target:"relay"}`, fire-and-forget). No-op when the blob is empty (a plain peer)
/// or when no socket is live (the next reconnect re-announces).
async fn send_announce(sink: &SharedSink, connected: &AtomicBool, identity: &SharedIdentity) {
    let attrs = identity.lock().expect("identity poisoned").clone();
    let empty = match &attrs {
        Value::Object(m) => m.is_empty(),
        _ => true,
    };
    if empty {
        return;
    }
    let text = fantastic_io_bridge::codec::encode_frame(
        &json!({"type": "announce", "attrs": attrs, "target": "relay"}),
    );
    let _ = send_bounded(sink, connected, Message::Text(text)).await;
}

/// Send the relay's no-reply `keepalive` verb every `HEARTBEAT_SECS` while
/// connected, refreshing the peer's `last_seen` so it stays `green`.
/// python `_start_heartbeat`: cancel whatever beat is running, then spawn a new
/// one — and `heartbeat <= 0` means OFF. Called from the socket-open path ONLY:
/// python starts it in `_on_socket_open`, so a leg whose dial failed carries no
/// heartbeat task and no `relay_heartbeat` row in `ps`. Rust used to spawn it at
/// construction, so an unreachable relay showed a row python never has.
fn start_heartbeat(
    spawn: &RelaySpawn,
    sink: &SharedSink,
    closed: &Arc<AtomicBool>,
    connected: &Arc<AtomicBool>,
    stop: &Arc<tokio::sync::Notify>,
    every_secs: f64,
) {
    if every_secs <= 0.0 {
        return;
    }
    // python cancels `_hb_task` before replacing it — one beat per transport.
    stop.notify_waiters();
    spawn.spawn(
        "relay_heartbeat",
        heartbeat_loop(
            Arc::clone(sink),
            Arc::clone(closed),
            Arc::clone(connected),
            Arc::clone(stop),
            every_secs,
        ),
    );
}

async fn heartbeat_loop(
    sink: SharedSink,
    closed: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    stop: Arc<tokio::sync::Notify>,
    every_secs: f64,
) {
    // python sends `json.dumps({"type": "keepalive"})` — WITH the space.
    let beat = fantastic_io_bridge::codec::encode_frame(&json!({"type": "keepalive"}));
    let period = Duration::from_secs_f64(every_secs);
    loop {
        tokio::select! {
            () = tokio::time::sleep(period) => {}
            () = stop.notified() => return,
        }
        if closed.load(Ordering::SeqCst) {
            return;
        }
        if !connected.load(Ordering::SeqCst) {
            continue;
        }
        let _ = send_bounded(&sink, &connected, Message::Text(beat.clone())).await;
    }
}

/// A binary relay event from our partner → the inner bridge frame: its body one
/// step down, the envelope's `["payload", …p]` becoming the inner frame's `p`.
/// A delivery whose `payload` is not an object is skipped (python
/// `isinstance(payload, dict)`); a body that did not sit inside it is not part of
/// the inner frame, which is then a plain envelope.
fn unwrap_binary(f: BinaryFrame, partner: &str) -> Option<Frame> {
    let BinaryFrame {
        mut head,
        path,
        body,
    } = f;
    if head.get("type").and_then(Value::as_str) != Some("event")
        || head.get("source").and_then(Value::as_str) != Some(partner)
    {
        return None;
    }
    let inner = head
        .get_mut("payload")
        .map(Value::take)
        .unwrap_or(Value::Null);
    if !inner.is_object() {
        return None;
    }
    Some(
        match path.and_then(|p| Blob::new(p, body).under("payload")) {
            Some(b) => Frame::Binary(inner, b),
            None => Frame::Text(inner),
        },
    )
}

#[async_trait]
impl BridgeTransport for RelayTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        send_to_partner(&self.sink, &self.connected, &self.partner, frame).await
    }

    async fn send_binary(&self, header: Value, body: Blob) -> Result<(), TransportError> {
        // the relay `send` nests the frame under its `payload`, the body's slot
        // one step down with it
        let env = json!({"type":"send","target":self.partner,"payload":header});
        let wire = encode_binary_frame(&env, &body.nested_in("payload"), &body.bytes);
        send_bounded(&self.sink, &self.connected, Message::Binary(wire))
            .await
            .map_err(TransportError::from)
    }

    async fn recv_frame(&self) -> Result<Frame, TransportError> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(r) => r,
            None => Err(TransportError::ConnectionClosed(
                "relay reader ended".into(),
            )),
        }
    }

    /// python `RelayTransport.closed` — EXPLICIT close only; a transient drop is
    /// healed by `recv` (reconnect), so the leg stays alive across blips.
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// python `RelayTransport.is_live`.
    fn is_live(&self) -> bool {
        RelayTransport::is_live(self)
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // ABORT FIRST, then reach for the sink. The supervisor and the heartbeat
        // both take the sink lock; with a write stalled on an unreadable peer,
        // taking it here first meant `close()` — and therefore `delete_agent` on
        // the bridge — could never return. Aborting the two owners leaves only
        // this one bounded wait.
        if let Some(h) = self.supervisor.lock().await.take() {
            h.abort();
        }
        // `notify_one` (not `notify_waiters`): it STORES a permit, so a heartbeat
        // that has not parked yet still ends at its first await.
        self.stop_heartbeat.notify_one();
        {
            let mut guard = self.sink.lock().await;
            if let Some(mut s) = guard.take() {
                let _ = tokio::time::timeout(SEND_TIMEOUT, s.close()).await;
            }
        }
        for (_id, tx) in self.relay_pending.lock().unwrap().drain() {
            let _ = tx.send(json!({"error":"relay_connector: closed","reason":"transport_closed"}));
        }
    }

    // ── directory surface (relay-specific) ─────────────────────────

    async fn list_peers(&self, timeout_s: f64) -> Value {
        self.relay_request(
            json!({"type":"call","payload":{"type":"list_peers"}}),
            timeout_s,
        )
        .await
    }

    async fn watch_directory(&self, timeout_s: f64) -> Value {
        self.relay_request(json!({"type":"watch"}), timeout_s).await
    }

    async fn unwatch_directory(&self) -> Value {
        let _ = send_bounded(
            &self.sink,
            &self.connected,
            Message::Text(fantastic_io_bridge::codec::encode_frame(
                &json!({"type":"unwatch","target":"relay"}),
            )),
        )
        .await;
        json!({"ok": true, "unwatched": "relay"})
    }

    async fn set_identity(&self, attrs: Value) -> Value {
        *self.identity.lock().expect("identity poisoned") = attrs.clone();
        send_announce(&self.sink, &self.connected, &self.identity).await;
        json!({"ok": true, "attrs": attrs})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader for a relay that checks no password (the fake ones here).
    fn no_password() -> PasswordReader {
        Arc::new(|| Box::pin(async { Ok(String::new()) }))
    }

    /// python starts the beat in `_on_socket_open`, so a leg whose eager dial
    /// FAILED has no `relay_heartbeat` task at all — and no `ps` row for one.
    /// Rust spawned it at construction, so an unreachable relay carried a row
    /// python never shows (caught by the `kernel ps` wire probe).
    #[tokio::test]
    async fn a_failed_dial_starts_no_heartbeat() {
        let kernel = Arc::new(Kernel::new());
        let t = RelayTransport::connect(
            "ws://127.0.0.1:1",
            "A",
            no_password(),
            "B",
            60.0,
            1.0,
            json!({}),
            RelaySpawn::new(&kernel, &AgentId::from("relay")),
        )
        .await
        .expect("reconnect > 0 swallows the first dial failure");
        let rows = kernel.tasks.rows();
        assert!(
            rows.iter().all(|r| r["verb"] != "relay_heartbeat"),
            "a beat was spawned with no socket: {rows:?}"
        );
        assert!(!t.is_live());
    }

    /// python: a failed EAGER dial is retried AT ONCE (`recv()` → `_reconnect_ws`
    /// dials before any sleep), and the first directory request id is `dir_1`
    /// (`self._relay_next_id += 1`). The fake relay refuses the first TCP
    /// connection and serves the second; with a 60 s backoff only an immediate
    /// re-dial can come up within the test's window.
    #[tokio::test]
    async fn a_failed_eager_dial_redials_at_once_and_directory_ids_start_at_one() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = oneshot::channel::<Value>();
        let server = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            drop(first); // the eager dial fails
            let (stream, _) = listener.accept().await.unwrap();
            #[allow(clippy::result_large_err)]
            let cb = |_req: &Request, mut resp: Response| {
                resp.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    HeaderValue::from_static(SUBPROTOCOL),
                );
                Ok(resp)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(stream, cb)
                .await
                .unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["target"] == "relay" && v["type"] == "call" {
                        let reply = json!({"type":"reply","id":v["id"],"data":{"peers":[]}});
                        ws.send(Message::Text(reply.to_string())).await.unwrap();
                        let _ = seen_tx.send(v);
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let kernel = Arc::new(Kernel::new());
        let t = RelayTransport::connect(
            &format!("ws://127.0.0.1:{port}"),
            "A",
            no_password(),
            "B",
            60.0,
            0.0,
            json!({}),
            RelaySpawn::new(&kernel, &AgentId::from("relay")),
        )
        .await
        .expect("reconnect > 0 swallows the first dial failure");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !t.is_live() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            t.is_live(),
            "the leg waited out its backoff before re-dialling"
        );
        let peers = t.list_peers(5.0).await;
        assert_eq!(peers, json!({"peers": []}), "{peers}");
        let req = seen_rx.await.expect("the directory call reached the relay");
        assert_eq!(req["id"], "dir_1");
        t.close().await;
        server.abort();
    }

    /// python `send()`: no socket raises `ConnectionClosed("relay_connector: not
    /// connected")`; a failed write raises `ConnectionClosed(str(e))` — the
    /// library's text with no prefix.
    #[tokio::test]
    async fn send_failure_texts_are_pythons() {
        let sink: SharedSinkOf<StalledSink> = Arc::new(Mutex::new(None));
        let connected = AtomicBool::new(false);
        let err = send_bounded(&sink, &connected, Message::Text("x".into()))
            .await
            .expect_err("no socket");
        match TransportError::from(err) {
            TransportError::ConnectionClosed(m) => {
                assert_eq!(m, "relay_connector: not connected");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classify_text_routes_partner_directory_and_replies() {
        let pending: RelayPending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        // partner event → the inner bridge frame.
        let bridge = json!({"type":"call","id":"A:1","target":"x","payload":{"type":"reflect"}});
        let event = json!({"type":"event","source":"B","payload": bridge.clone()});
        assert_eq!(
            classify_text(event, "B", &pending).unwrap().into_value(),
            bridge
        );
        // directory event (source:"relay") → a bridge `event` frame from the
        // directory, for the inbox.
        let dir = json!({"type":"event","source":"relay","payload":{"type":"peer_status","guid":"C","status":"yellow"}});
        let f = classify_text(dir, "B", &pending).unwrap().into_value();
        assert_eq!(
            f,
            json!({"type":"event","src":"relay","payload":{"type":"peer_status","guid":"C","status":"yellow"}})
        );
        // a foreign peer → skipped.
        assert!(classify_text(
            json!({"type":"event","source":"X","payload":{}}),
            "B",
            &pending
        )
        .is_none());
        // a relay reply → resolves the matching pending request, no frame.
        let (tx, mut rx) = oneshot::channel::<Value>();
        pending.lock().unwrap().insert("dir_0".into(), tx);
        assert!(classify_text(
            json!({"type":"reply","id":"dir_0","data":{"peers":[]}}),
            "B",
            &pending
        )
        .is_none());
        assert_eq!(rx.try_recv().unwrap(), json!({"peers":[]}));
    }

    #[test]
    fn binary_round_trips_raw_body_through_relay_envelope() {
        use fantastic_kernel::Step;
        let inner = json!({"type":"reply","id":"A:7","data":{"bytes":null}});
        let env = json!({"type":"event","source":"B","payload": inner});
        let body: Vec<u8> = (0u8..=255).collect();
        let path = vec![
            Step::from("payload"),
            Step::from("data"),
            Step::from("bytes"),
        ];
        let wire = encode_binary_frame(&env, &path, &body);
        let frame = unwrap_binary(decode_binary_frame(&wire).unwrap(), "B").unwrap();
        match frame {
            Frame::Binary(header, got) => {
                assert_eq!(got.bytes, body);
                assert_eq!(got.path, vec![Step::from("data"), Step::from("bytes")]);
                assert_eq!(header, inner);
            }
            Frame::Text(_) => panic!("expected a binary frame"),
        }
        // a LITERAL dotted key stays one step through the tunnel
        let inner = json!({"type":"call","id":"A:8","payload":{"a":{"b":"kept"},"a.b":null}});
        let env = json!({"type":"event","source":"B","payload": inner});
        let path = vec![
            Step::from("payload"),
            Step::from("payload"),
            Step::from("a.b"),
        ];
        let wire = encode_binary_frame(&env, &path, b"x");
        match unwrap_binary(decode_binary_frame(&wire).unwrap(), "B").unwrap() {
            Frame::Binary(header, got) => {
                assert_eq!(got.path, vec![Step::from("payload"), Step::from("a.b")]);
                assert_eq!(header, inner);
            }
            Frame::Text(_) => panic!("expected a binary frame"),
        }
        let foreign = BinaryFrame {
            head: json!({"type":"event","source":"C","payload":{}}),
            path: None,
            body: vec![],
        };
        assert!(unwrap_binary(foreign, "B").is_none());
    }
    // ── the stalled peer (#88) ──────────────────────────────────────

    /// A sink whose `poll_ready` never completes — the network partition where
    /// the relay stopped reading and our kernel send buffer filled. Before the
    /// deadline this parked the sink `Mutex` forever, so every other forward, the
    /// keepalive and `close()` (hence `delete_agent` on the bridge) hung on it.
    struct StalledSink;

    impl futures_util::Sink<Message> for StalledSink {
        type Error = std::io::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }
        fn start_send(self: std::pin::Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }
    }

    #[tokio::test]
    async fn a_stalled_sink_times_out_drops_the_sink_and_marks_disconnected() {
        let sink: SharedSinkOf<StalledSink> = Arc::new(Mutex::new(Some(StalledSink)));
        let connected = AtomicBool::new(true);
        let started = std::time::Instant::now();
        let err = send_bounded(&sink, &connected, Message::Text("x".into()))
            .await
            .expect_err("a stalled peer must not park the writer forever");
        assert!(
            matches!(err, SendFail::Failed(ref m) if m.contains("timed out")),
            "expected a timeout failure"
        );
        // Bounded, and by the DEADLINE — not by luck.
        assert!(
            started.elapsed() < SEND_TIMEOUT * 4,
            "{:?}",
            started.elapsed()
        );
        // The sink is DROPPED, so the lock is free for good and the supervisor
        // re-dials instead of the whole transport wedging behind one write.
        assert!(sink.lock().await.is_none(), "the dead sink was kept");
        assert!(
            !connected.load(Ordering::SeqCst),
            "the leg still claims connected"
        );
        // And the lock really is free: the next writer gets it immediately.
        assert!(matches!(
            send_bounded(&sink, &connected, Message::Text("y".into())).await,
            Err(SendFail::NotConnected)
        ));
    }

    #[tokio::test]
    async fn close_returns_and_aborts_the_owners_of_the_sink_lock() {
        // `close()` used to take the sink lock BEFORE aborting the supervisor and
        // the heartbeat — both of which take that same lock to write. With a write
        // stalled, `close()` could never return, so the bridge agent could not be
        // deleted. It now aborts first and settles the directory waiters.
        let supervisor = tokio::spawn(async { std::future::pending::<()>().await });
        let stop = Arc::new(tokio::sync::Notify::new());
        let beat_stop = Arc::clone(&stop);
        let heartbeat = tokio::spawn(async move { beat_stop.notified().await });
        let (_tx, rx) = mpsc::channel::<Result<Frame, TransportError>>(1);
        let relay_pending: RelayPending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (dir_tx, dir_rx) = oneshot::channel::<Value>();
        relay_pending
            .lock()
            .unwrap()
            .insert("dir_1".to_string(), dir_tx);
        let t = RelayTransport {
            partner: "B".to_string(),
            sink: Arc::new(Mutex::new(None)),
            rx: Mutex::new(rx),
            closed: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(true)),
            supervisor: Mutex::new(Some(supervisor)),
            stop_heartbeat: Arc::clone(&stop),
            relay_pending,
            relay_next_id: AtomicU64::new(0),
            identity: Arc::new(std::sync::Mutex::new(json!({}))),
        };
        tokio::time::timeout(Duration::from_secs(2), t.close())
            .await
            .expect("close() must return");
        assert!(t.closed.load(Ordering::SeqCst));
        assert!(
            t.supervisor.lock().await.is_none(),
            "supervisor not aborted"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(2), heartbeat)
                .await
                .is_ok(),
            "the heartbeat task must end on close"
        );
        let settled = dir_rx
            .await
            .expect("a pending directory request must settle");
        assert_eq!(settled["reason"], "transport_closed");
    }
}
