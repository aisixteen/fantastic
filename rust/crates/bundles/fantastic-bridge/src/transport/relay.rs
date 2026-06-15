//! Relay-kernel router transport — `tokio-tungstenite` to `../fantastic_relay`.
//!
//! Mirrors `python/bundled_agents/io/relay_connector/_relay.py::RelayTransport`.
//! The relay is itself a kernel: we dial `ws://<host>/<guid>` (subprotocol
//! `fantastic.relay.v1`, header `X-Fantastic-Auth: <group password>` checked once
//! at the WS upgrade) and reach a fixed `partner` GUID. The relay routes by
//! `target` and delivers peer→peer ONE-WAY as `{type:"event", source, payload}`.
//!
//! So this transport TUNNELS the shared bridge frames: `send` wraps each frame in
//! a relay envelope `{type:"send", target:<partner>, payload:<frame>}`; `recv`
//! accepts only `{type:"event", source==partner}` and returns the inner frame.
//! PURE STREAMS via the shared codec: a control frame is a TEXT WS frame, a
//! `read_stream` chunk a BINARY WS frame `[4B len|header|body]` — raw bytes, NO
//! base64. For a binary frame the `_binary_path` is lifted to `payload.<path>`.
//!
//! Resilience: the connection is SELF-HEALING. A supervisor task owns the socket
//! — on a drop it re-dials after `reconnect` seconds (default 10; `0` = one-shot)
//! and swaps in the new sink, transparently to the engine's read loop. The
//! initial dial is eager; if it fails with reconnect on, the leg still boots and
//! connects in the background. A keepalive task sends the relay's `keepalive`
//! verb to stay `green`.

use super::{BridgeTransport, Frame, TransportError};
use async_trait::async_trait;
use fantastic_io_bridge::codec::{decode_binary_frame, encode_binary_frame};
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
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
type SharedSink = Arc<Mutex<Option<WsSink>>>;
/// Outstanding relay-LEVEL requests (the directory `call`/`watch target:relay`),
/// keyed by minted id. Distinct from the engine's partner bridge-frame correlation.
type RelayPending = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<Value>>>>;

/// The WS subprotocol the relay pairs + authenticates on.
pub const SUBPROTOCOL: &str = "fantastic.relay.v1";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_SECS: u64 = 30;

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
    heartbeat: Mutex<Option<JoinHandle<()>>>,
    relay_pending: RelayPending,
    relay_next_id: AtomicU64,
}

/// Build the WS upgrade request (path GUID + subprotocol + auth header) and dial.
async fn dial(url: &str, token: &str) -> Result<WsStream, TransportError> {
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
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, connect_async(req)).await {
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
    pub async fn connect(
        relay_url: &str,
        guid: &str,
        token: &str,
        partner_guid: &str,
        reconnect_secs: f64,
    ) -> Result<Arc<Self>, TransportError> {
        let url = format!("{}/{}", relay_url.trim_end_matches('/'), guid);
        let token = token.to_string();
        let partner = partner_guid.to_string();
        let sink: SharedSink = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel::<Result<Frame, TransportError>>(64);
        let closed = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let relay_pending: RelayPending = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // Eager first dial. On success seed the initial source; on failure with
        // reconnect off, fail the boot; otherwise heal in the background.
        let initial: Option<SplitStream<WsStream>> = match dial(&url, &token).await {
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

        let supervisor = tokio::spawn(supervise(
            url,
            token,
            partner.clone(),
            Arc::clone(&sink),
            tx,
            Arc::clone(&closed),
            Arc::clone(&connected),
            Arc::clone(&relay_pending),
            reconnect_secs,
            initial,
        ));
        let heartbeat = tokio::spawn(heartbeat_loop(
            Arc::clone(&sink),
            Arc::clone(&closed),
            Arc::clone(&connected),
        ));

        Ok(Arc::new(Self {
            partner,
            sink,
            rx: Mutex::new(rx),
            closed,
            connected,
            supervisor: Mutex::new(Some(supervisor)),
            heartbeat: Mutex::new(Some(heartbeat)),
            relay_pending,
            relay_next_id: AtomicU64::new(0),
        }))
    }

    /// Send a relay-LEVEL frame to the directory (`target:"relay"` + a minted id)
    /// and await the correlated `{type:"reply", id, data}`. Bypasses the partner
    /// tunnel. Returns an error object if disconnected / timed out.
    async fn relay_request(&self, mut frame: Value, timeout_s: f64) -> Value {
        let id = format!("dir_{}", self.relay_next_id.fetch_add(1, Ordering::SeqCst));
        frame["id"] = json!(id);
        frame["target"] = json!("relay");
        let (tx, rx) = oneshot::channel::<Value>();
        self.relay_pending
            .lock()
            .expect("relay_pending poisoned")
            .insert(id.clone(), tx);
        {
            let text = frame.to_string();
            let mut guard = self.sink.lock().await;
            match guard.as_mut() {
                Some(s) => {
                    if s.send(Message::Text(text)).await.is_err() {
                        self.relay_pending.lock().unwrap().remove(&id);
                        return json!({"error":"relay_connector: directory send failed","reason":"transport_error"});
                    }
                }
                None => {
                    self.relay_pending.lock().unwrap().remove(&id);
                    return json!({"error":"relay_connector: not connected","reason":"not_connected"});
                }
            }
        }
        match tokio::time::timeout(Duration::from_secs_f64(timeout_s), rx).await {
            Ok(Ok(v)) => v,
            _ => {
                self.relay_pending.lock().unwrap().remove(&id);
                json!({"error":"relay_connector: directory timeout","reason":"timeout"})
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
            Ok(Message::Text(t)) => match serde_json::from_str::<Value>(&t) {
                Ok(env) => classify_text(env, partner, relay_pending),
                Err(_) => None,
            },
            // Binary frames are partner stream chunks only (directory events are text).
            Ok(Message::Binary(b)) => match decode_binary_frame(&b) {
                Ok((env, body)) => unwrap_binary(env, body, partner),
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

/// Classify a text relay frame. Resolves a directory `reply` (side effect, no
/// frame); surfaces a `source:"relay"` event as `{type:"event", payload}`;
/// unwraps a `source:partner` delivery. Returns None for anything else.
fn classify_text(env: Value, partner: &str, relay_pending: &RelayPending) -> Option<Frame> {
    match env.get("type").and_then(Value::as_str) {
        Some("reply") => {
            if let Some(id) = env.get("id").and_then(Value::as_str) {
                if let Some(tx) = relay_pending
                    .lock()
                    .expect("relay_pending poisoned")
                    .remove(id)
                {
                    let _ = tx.send(env.get("data").cloned().unwrap_or(Value::Null));
                }
            }
            None
        }
        Some("event") => match env.get("source").and_then(Value::as_str) {
            // Directory event → re-emit on the connector inbox via the engine.
            Some("relay") => Some(Frame::Text(json!({
                "type": "event",
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
    token: String,
    partner: String,
    sink: SharedSink,
    tx: mpsc::Sender<Result<Frame, TransportError>>,
    closed: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    relay_pending: RelayPending,
    reconnect_secs: f64,
    mut initial: Option<SplitStream<WsStream>>,
) {
    let backoff = Duration::from_secs_f64(reconnect_secs.max(0.0));
    loop {
        if closed.load(Ordering::SeqCst) {
            break;
        }
        let source = if let Some(s) = initial.take() {
            s // first iteration: the eager connection
        } else {
            tokio::time::sleep(backoff).await;
            if closed.load(Ordering::SeqCst) {
                break;
            }
            match dial(&url, &token).await {
                Ok(stream) => {
                    let (s, source) = stream.split();
                    *sink.lock().await = Some(s);
                    connected.store(true, Ordering::SeqCst);
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

/// Send the relay's no-reply `keepalive` verb every `HEARTBEAT_SECS` while
/// connected, refreshing the peer's `last_seen` so it stays `green`.
async fn heartbeat_loop(sink: SharedSink, closed: Arc<AtomicBool>, connected: Arc<AtomicBool>) {
    let beat = serde_json::to_string(&json!({"type": "keepalive"})).unwrap();
    loop {
        tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECS)).await;
        if closed.load(Ordering::SeqCst) {
            return;
        }
        if !connected.load(Ordering::SeqCst) {
            continue;
        }
        let mut guard = sink.lock().await;
        if let Some(s) = guard.as_mut() {
            let _ = s.send(Message::Text(beat.clone())).await;
        }
    }
}

/// A binary relay event from our partner → `Frame::Binary(inner header, body)`,
/// shifting the envelope's `_binary_path` (`payload.<p>`) back to `<p>`.
fn unwrap_binary(mut env: Value, body: Vec<u8>, partner: &str) -> Option<Frame> {
    if env.get("type").and_then(Value::as_str) != Some("event")
        || env.get("source").and_then(Value::as_str) != Some(partner)
    {
        return None;
    }
    let env_path = env
        .get("_binary_path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut inner = env
        .get_mut("payload")
        .map(Value::take)
        .unwrap_or(Value::Null);
    if let Some(p) = env_path.strip_prefix("payload.") {
        if let Value::Object(m) = &mut inner {
            m.insert("_binary_path".into(), json!(p));
        }
    }
    Some(Frame::Binary(inner, body))
}

#[async_trait]
impl BridgeTransport for RelayTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        let env = json!({"type":"send","target":self.partner,"payload":frame});
        let text = serde_json::to_string(&env)
            .map_err(|e| TransportError::Other(format!("relay encode: {e}")))?;
        let mut guard = self.sink.lock().await;
        match guard.as_mut() {
            Some(s) => s
                .send(Message::Text(text))
                .await
                .map_err(|e| TransportError::ConnectionClosed(format!("relay send: {e}"))),
            None => Err(TransportError::ConnectionClosed(
                "relay not connected".into(),
            )),
        }
    }

    async fn send_binary(&self, header: Value, body: Vec<u8>) -> Result<(), TransportError> {
        let inner_path = header
            .get("_binary_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut inner = header;
        if let Value::Object(m) = &mut inner {
            m.remove("_binary_path");
        }
        let mut env = json!({"type":"send","target":self.partner,"payload":inner});
        if !inner_path.is_empty() {
            env["_binary_path"] = json!(format!("payload.{inner_path}"));
        }
        let wire = encode_binary_frame(&env, &body);
        let mut guard = self.sink.lock().await;
        match guard.as_mut() {
            Some(s) => s
                .send(Message::Binary(wire))
                .await
                .map_err(|e| TransportError::ConnectionClosed(format!("relay send binary: {e}"))),
            None => Err(TransportError::ConnectionClosed(
                "relay not connected".into(),
            )),
        }
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

    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        {
            let mut guard = self.sink.lock().await;
            if let Some(mut s) = guard.take() {
                let _ = s.close().await;
            }
        }
        if let Some(h) = self.supervisor.lock().await.take() {
            h.abort();
        }
        if let Some(h) = self.heartbeat.lock().await.take() {
            h.abort();
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
        let mut guard = self.sink.lock().await;
        if let Some(s) = guard.as_mut() {
            let _ = s
                .send(Message::Text(
                    json!({"type":"unwatch","target":"relay"}).to_string(),
                ))
                .await;
        }
        json!({"ok": true, "unwatched": "relay"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // directory event (source:"relay") → a bridge `event` frame for the inbox.
        let dir = json!({"type":"event","source":"relay","payload":{"type":"peer_status","guid":"C","status":"yellow"}});
        let f = classify_text(dir, "B", &pending).unwrap().into_value();
        assert_eq!(f["type"], "event");
        assert_eq!(f["payload"]["type"], "peer_status");
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
        let inner = json!({"type":"reply","id":"A:7","data":{"bytes":null}});
        let env = json!({
            "type":"event","source":"B","payload": inner,
            "_binary_path":"payload.data.bytes"
        });
        let body: Vec<u8> = (0u8..=255).collect();
        let wire = encode_binary_frame(&env, &body);
        let (decoded_env, decoded_body) = decode_binary_frame(&wire).unwrap();
        let frame = unwrap_binary(decoded_env, decoded_body, "B").unwrap();
        match frame {
            Frame::Binary(header, got_body) => {
                assert_eq!(got_body, body);
                assert_eq!(header["_binary_path"], "data.bytes");
                assert_eq!(header["type"], "reply");
            }
            _ => panic!("expected a binary frame"),
        }
        let foreign = json!({"type":"event","source":"C","payload":{}});
        assert!(unwrap_binary(foreign, vec![], "B").is_none());
    }
}
