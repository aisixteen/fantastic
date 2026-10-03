//! The WebSocket frame loop — the rust port of python's
//! `PB/io/web_ws/src/web_ws/_proxy.py::run`.
//!
//! ```text
//! C→S {type:"call",  target, payload, id, secret?}
//!                                             → kernel.send → reply
//!                                               (`secret` = the item's marks —
//!                                               dotted strings and/or arrays
//!                                               of steps, `fantastic_kernel::secret`)
//! C→S {type:"emit",  target, payload}         → kernel.emit (no reply)
//! C→S {type:"watch"|"unwatch", src}           → (un)register a watcher
//! C→S {type:"state_subscribe"|"state_unsubscribe"}
//! C→S {type:"hello", guid}                    → the page kernel becomes the
//!                                               host's peer `<guid>|<agent>`
//! S→C {type:"reply", id, data} | {type:"error", id, …}
//! S→C {type:"event", src, payload}            → an event from a watched agent
//!                                               (`src` = the name the client watched)
//! S→C {type:"event", payload}                 → an event emitted to THIS connection
//! S→C {type:"call", id:"h<n>", target, payload, caller, secret?}
//!                                             → the host calling a page agent
//! C→S {type:"reply"|"error", id:"h<n>", data} → that call's outcome
//! S→C {type:"emit", target, payload, caller?}  → a host emit to a page agent
//!                                               (every tab of the guid)
//! S→C {type:"reply_mirror", id, data}          → a reply this leg sent ANOTHER
//!                                               tab of this connection's guid
//! ```
//!
//! **CROSS-KERNEL ADDRESSING (owner, 2026-09-30).** After `hello`, the host
//! kernel reaches the page's agents as `<guid>|<agent>` with an ordinary `send`;
//! the call rides this socket as an outbound `call` frame and the page's answer
//! is its outcome. This leg only carries the frames and registers the peer with
//! the kernel ([`fantastic_kernel::PeerRoute`]) — the host kernel routes and gates
//! egress, the page kernel judges its own agents' ingress. See [`peer`].
//!
//! Every server→client TEXT frame leaves through
//! [`fantastic_io_bridge::codec::encode_frame`] — python's
//! `json.dumps(envelope, default=str)`, `, `/`: ` separators and
//! `ensure_ascii` — so a `·` or a `↑` in a reply is `·` / `↑` on the
//! wire exactly as python sends it (A13.14).
//!
//! **IDENTITY (C40).** Every frame runs as the web_ws LEG (`leg_agent_id`), the
//! child agent whose record carries the `ingress_rule` this face is gated by —
//! never as the URL host and never as the operator. The rust loop used to
//! dispatch `call` with no sender at all on the binary path and as the HOST on
//! the text path, so a browser inherited the origin root of trust: measured
//! 2026-09-16, a private agent, an outsider, and a SEALED leg all answered a
//! `call` over the socket. The two tasks the loop registers still belong to the
//! HOST (`ws_drain`, `ws_call`), which is what python's `spawn_task(host_id, …)`
//! does.
//!
//! **THE LEG GATE.** Python resolves the leg's ingress rule PER FRAME, so
//! opening the edge (`update_agent ingress_rule=allow_all_ingress`) takes effect
//! without a reconnect, and an ABSENT rule is SEALED. A denied `call` is
//! answered (`{error, reason:'unauthorized', hint?, see?}` inside the reply's
//! `data`) because discovery-through-denial is the point; every other kind is
//! dropped silently, the connect-time auto-watch included — a sealed leg that
//! auto-watched leaked the host's whole event stream to anyone who completed the
//! handshake.

use crate::AppState;
use fantastic_io_bridge::authorizer::{ingress, manifold_spec, Action, Decision};
use fantastic_io_bridge::codec::{decode_binary_frame, encode_frame};
use fantastic_io_bridge::ws_deflate::DeflateStream;
use fantastic_kernel::{AgentId, Item, Kernel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

#[path = "ws_peer.rs"]
mod peer;

/// uvicorn's `ws_ping_interval` (`config.py:191`) — the server pings an idle
/// socket every 20 s.
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(20);
/// uvicorn's `ws_ping_timeout` (`config.py:192`) — no pong inside this window
/// closes the socket `1011 keepalive ping timeout`
/// (`websockets/legacy/protocol.py:1213-1252`).
pub(crate) const PING_TIMEOUT: Duration = Duration::from_secs(20);
/// The close code websockets uses when the keepalive ping goes unanswered.
pub(crate) const CLOSE_INTERNAL_ERROR: u16 = 1011;
/// The close reason websockets sends with it.
pub(crate) const KEEPALIVE_TIMEOUT_REASON: &str = "keepalive ping timeout";

/// The envelope key a reply's body sits under — python's `find_bytes_path` walks
/// `{type:reply, id, data:{…bytes…}}` and finds the chunk below `data`, at the
/// reply's own slot (`["data", "bytes"]` for a `read_stream`).
const REPLY_KEY: &str = "data";

/// The envelope key an inbound binary frame's body must sit under for it to
/// reach the target — at ANY path there, which is where python's `set_path` puts
/// it before `kernel.send` (`["payload", "data"]` for `paste_image`,
/// `["payload", "bytes"]` for `write_stream`, `["payload", "a.b"]` for the key
/// `"a.b"`).
const INBOUND_PAYLOAD_KEY: &str = "payload";

/// One upgraded socket: tungstenite over the byte stream that carries the
/// negotiated permessage-deflate (see `handlers::ws_upgrade`).
pub(crate) type Socket = tokio_tungstenite::WebSocketStream<
    DeflateStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>,
>;

/// One frame leaving the socket. Text and binary share one single-consumer
/// sink, and the control frames (keepalive ping, the close echo) ride the same
/// channel so they cannot overtake a queued reply.
enum Out {
    Text(String),
    Binary(Vec<u8>),
    Ping,
    Close(Option<CloseFrame<'static>>),
}

/// Track one in-flight `call` dispatch, PRUNING the finished ones first.
///
/// Nothing removed a finished handle until the socket closed, so the vector
/// only ever grew: a dashboard tab polling `status` once a second accumulated
/// ~86k handles over a day. LIVENESS COMES FROM THE TABLE, which holds only
/// running work.
pub(crate) fn track_call(
    kernel: &Arc<Kernel>,
    pending: &std::sync::Mutex<Vec<fantastic_kernel::TaskId>>,
    id: fantastic_kernel::TaskId,
) {
    let mut p = pending.lock().expect("pending poisoned");
    p.retain(|t| kernel.tasks.contains(t));
    p.push(id);
}

/// python `_gate` — the leg's INGRESS rule, resolved off the LEG's record on
/// every frame. A missing record, or a record with no rule, is SEALED.
///
/// `kind` ∈ call|emit|watch|state_subscribe; for `call` the verb is the
/// payload's `type`, otherwise the verb IS the kind (python `gate_inbound`).
/// The credential rides the frame ENVELOPE (`auth_token`), never the payload.
fn gate_leg(kernel: &Arc<Kernel>, leg: &AgentId, kind: &str, frame: &Value) -> Decision {
    let (primary, auth) = match kernel.agents.get(leg).map(|e| Arc::clone(&e)) {
        Some(a) => {
            let meta = a.meta.read().expect("meta poisoned");
            (meta.get("ingress_rule").cloned(), meta.get("auth").cloned())
        }
        None => (None, None),
    };
    let inner = frame.get("payload").filter(|v| v.is_object());
    let verb = if kind == "call" {
        inner
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
    } else {
        kind
    };
    // python `frame.get("target") or frame.get("src") or ""` — a falsy target
    // falls through to `src`, which is what a watch frame carries.
    let addr = frame
        .get("target")
        .filter(|v| fantastic_bundle::py_truthy(v))
        .or_else(|| frame.get("src"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let spec = manifold_spec(primary.as_ref(), auth.as_ref(), "_ingress");
    ingress::resolve(spec.as_ref()).authorize(&Action {
        kind,
        target: addr,
        verb,
        token: frame.get("auth_token").and_then(Value::as_str),
    })
}

/// python `bad("frame", "frame", "a well-formed call/emit/watch envelope")`
/// under a `type: error` head — the ONE answer to every undecodable frame, sent
/// BEFORE any gate (a sealed leg answers it too) and with the socket KEPT.
fn frame_error() -> Value {
    let mut out = Map::new();
    out.insert("type".to_string(), json!("error"));
    let bad =
        fantastic_kernel::refusal::bad("frame", "frame", "a well-formed call/emit/watch envelope");
    if let Value::Object(m) = bad {
        for (k, v) in m {
            out.insert(k, v);
        }
    }
    Value::Object(out)
}

/// A decoded inbound frame: the envelope, plus the raw body when the frame
/// carried one under its `payload` or its `data`.
struct Inbound {
    envelope: Value,
    /// `Some` when `_binary_path` named a slot under the frame's `payload` — the
    /// body AT that payload path (`Item::with_binary` carries both).
    body: Option<fantastic_kernel::Blob>,
    /// `Some` when `_binary_path` named the frame's `data` or a slot under it — a
    /// page ANSWERING one of the host's calls with bytes; the path is the
    /// envelope's (`["data", …]`), the answer re-roots it at the reply.
    answer_body: Option<fantastic_kernel::Blob>,
}

/// python `decode_inbound` + `isinstance(frame, dict)`: parse a text frame as
/// JSON or a binary frame as `[4B len | header | body]`, pop `_binary_path` and
/// place the body there. `Err` is the ONE malformed-frame answer.
fn decode_inbound(msg: InboundRaw) -> Result<Inbound, ()> {
    let (envelope, path, bytes) = match msg {
        // STRICT (D13): python `decode_frame` → `io_bridge.loads`.
        InboundRaw::Text(t) => (
            fantastic_io_bridge::codec::loads(&t).map_err(|_| ())?,
            None,
            Vec::new(),
        ),
        // python `decode_frame` performs `set_path(head, path, body)` and a bad
        // path is the frame error; the codec places a `null` in the same slot
        // (rust cannot store the bytes in the `Value`) and keeps the body beside.
        InboundRaw::Binary(b) => {
            let f = decode_binary_frame(&b).map_err(|_| ())?;
            (f.head, f.path, f.body)
        }
    };
    if !envelope.is_object() {
        return Err(());
    }
    // No path ⇒ a plain envelope; a body that came with one is dropped, exactly
    // as python drops it (nothing names where it would go).
    let Some(path) = path else {
        return Ok(Inbound {
            envelope,
            body: None,
            answer_body: None,
        });
    };
    // A body under the frame's `payload` reaches the handler AT ITS PATH there
    // (`Item::with_binary` carries both). One under `data` is the reply body of
    // a host call into the page. Any other placed path keeps its `null` and the
    // body is dropped.
    let blob = fantastic_kernel::Blob::new(path, bytes);
    let (body, answer_body) = match blob.path.first() {
        Some(fantastic_kernel::Step::Key(k)) if k == INBOUND_PAYLOAD_KEY => {
            (blob.under(INBOUND_PAYLOAD_KEY), None)
        }
        Some(fantastic_kernel::Step::Key(k)) if k == REPLY_KEY => (None, Some(blob)),
        _ => (None, None),
    };
    Ok(Inbound {
        envelope,
        body,
        answer_body,
    })
}

/// A raw inbound WS message, before decoding.
enum InboundRaw {
    Text(String),
    Binary(Vec<u8>),
}

/// Send one server→client envelope, picking the frame kind from the CARRIER:
/// an envelope whose `key` value holds a body is a binary frame whose head
/// keeps `null` at the body's slot, in its python position, and names it in
/// `_binary_path` (`[key, …the body's own steps]`).
async fn send_envelope(
    out_tx: &tokio::sync::mpsc::Sender<Out>,
    envelope: Value,
    body: Option<fantastic_kernel::Blob>,
    key: &str,
) {
    let out = match body {
        None => Out::Text(encode_frame(&envelope)),
        Some(body) => Out::Binary(binary_out(envelope, &body, key)),
    };
    let _ = out_tx.send(out).await;
}

/// `envelope[key]` holding `body` at its slot, as one binary frame: `null` placed
/// there (in place when the slot exists, appended when not — python's
/// `set_path`), `_binary_path` naming `[key, …steps]`.
fn binary_out(mut envelope: Value, body: &fantastic_kernel::Blob, key: &str) -> Vec<u8> {
    let path = body.nested_in(key);
    // a carrier's slot resolves in its own value; a reply that is not an object
    // has none, and the frame then names the slot it would sit at
    let _ = fantastic_bundle::path::place(&mut envelope, &path, Value::Null);
    fantastic_io_bridge::codec::encode_binary_frame(&envelope, &path, &body.bytes)
}

/// One queued payload → one `{type:"event", payload}` frame on the OUTBOUND
/// sink — python's `drain_outbound` body. `false` once the sink is gone.
///
/// A QUEUED PAYLOAD HOLDING BYTES IS A BINARY FRAME — python's `encode_outbound`
/// finds the bytes inside the payload dict and sends `[4B len | head with
/// _binary_path | body]`. The rust carrier keeps the body beside the payload, so
/// the path is the item's own path nested under this envelope's `payload`.
async fn forward_event(
    out_tx: &tokio::sync::mpsc::Sender<Out>,
    item: fantastic_kernel::inbox::InboxItem,
    src: Option<&str>,
) -> bool {
    let frame = match src {
        None => json!({"type": "event", "payload": item.payload}),
        Some(s) => json!({"type": "event", "src": s, "payload": item.payload}),
    };
    match item.body {
        None => out_tx.send(Out::Text(encode_frame(&frame))).await.is_ok(),
        Some(body) => {
            send_envelope(out_tx, frame, Some(body), INBOUND_PAYLOAD_KEY).await;
            true
        }
    }
}

/// python `web_ws._proxy._make_client_id` — `f"_ws_{secrets.token_hex(3)}"`:
/// 3 bytes from the OS CSPRNG as 6 lowercase hex. The process-global counter
/// this replaced minted an ASCENDING `_ws_000000` family, a shape no python
/// consumer (or capture diff) expects. A failure is FATAL — no second source.
pub(crate) fn make_client_id() -> String {
    let mut buf = [0u8; 3];
    // SAFETY: `buf` is a valid writable buffer of exactly 3 bytes.
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr().cast(), buf.len()) };
    assert!(rc == 0, "getentropy failed: no randomness source");
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("_ws_{hex}")
}

/// WebSocket proxy loop.
///
/// `host_agent_id` is the URL the browser connected to
/// (`ws://host/<host_id>/ws`) — auto-watched on connect when the leg admits an
/// unauthenticated watch, and the agent both kernel tasks are registered under.
/// `leg_agent_id` is THIS web_ws surface's own id: the record the gate reads and
/// the sender every frame dispatches as.
pub(crate) async fn ws_loop(
    state: AppState,
    socket: Socket,
    host_agent_id: AgentId,
    leg_agent_id: AgentId,
) {
    use std::collections::HashMap;
    let client_id = AgentId::from(make_client_id().as_str());

    let (mut sink, mut stream) = socket.split();

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Out>(state.kernel.inbox_bound);
    // Hook the synthetic client inbox into the kernel. THE READER REGISTERS
    // FIRST, and the queue drops the OLDEST at the bound — a browser that
    // stalls catches up on the freshest frames rather than freezing on stale.
    //
    // ONE OUTBOUND ORDER, PYTHON'S. Both python and rust fan a payload into this
    // client's queue BEFORE the target's handler runs, so at the moment a reply
    // exists the watch event is already queued — and python's cooperative loop
    // puts it on the wire FIRST (the drain task was made runnable at the fanout,
    // long before `_on_call` resumed). Rust's drain is a PARALLEL task, so the
    // reply overtook the event it must follow (measured in the r3 capture at
    // three AI replies). The lock below makes flush-then-reply ONE step: every
    // payload queued when the handler settled reaches `out_tx` before the reply
    // does, with no sleep and no yield.
    let inbox_rx = Arc::new(tokio::sync::Mutex::new(
        state.kernel.ensure_inbox(&client_id),
    ));
    // A second read half on the SAME queue, used ONLY to WAIT: a drainer that
    // held the lock across its wait would block every flush forever.
    let inbox_ready = state.kernel.ensure_inbox(&client_id);
    // EVERY READER THIS SOCKET DRAINS — its own inbox (events emitted to the
    // connection, no `src`) and ONE PER WATCH (so each mirrored event goes out
    // stamped with the `src` it came from; a bridge on the other end re-emits
    // only what it subscribed to). A reply flushes them ALL first.
    let readers: Readers = Arc::new(std::sync::Mutex::new(vec![(None, Arc::clone(&inbox_rx))]));

    // THE CONNECT-TIME AUTO-WATCH IS GATED. Only mirror the host's inbox when
    // THIS leg's rule admits an inbound `watch` with NO credential (an open
    // `allow_all` leg). On a sealed or credentialed leg a client that merely
    // completed the handshake would otherwise receive every event the host sees
    // without ever passing the gate.
    let admits_autowatch = matches!(
        gate_leg(
            &state.kernel,
            &leg_agent_id,
            "watch",
            &json!({"src": host_agent_id.0}),
        ),
        Decision::Allow
    );
    // The name the client used → the watch it REALLY registered (see
    // [`watch_path`]): the resolved agent, that watch's own reader, its drain.
    let mut watching: HashMap<String, WatchEntry> = HashMap::new();

    // Pending `call` tasks — cancelled on disconnect so long-lived sends
    // release their locks immediately.
    let pending_calls: Arc<std::sync::Mutex<Vec<fantastic_kernel::TaskId>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // State-stream subscription tokens — removed on disconnect.
    let state_unsubs: Arc<std::sync::Mutex<Vec<fantastic_kernel::SubscriberToken>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // REGISTERED, under the HOST: python spawns the drain as the kernel task
    // `ws_drain` on `host_agent_id`, so an open socket is visible in `ps` and
    // its pump is cancellable like any other work.
    let drain_task = state
        .kernel
        .clone()
        .spawn_task(&host_agent_id, "ws_drain", {
            let out_tx = out_tx.clone();
            let inbox_rx = Arc::clone(&inbox_rx);
            async move {
                loop {
                    let mut gone = false;
                    {
                        let mut rx = inbox_rx.lock().await;
                        while let Some(item) = rx.try_recv() {
                            if !forward_event(&out_tx, item, None).await {
                                gone = true;
                                break;
                            }
                        }
                    }
                    if gone {
                        break;
                    }
                    inbox_ready.ready().await;
                }
            }
        });

    // Outbound forwarder: one single-consumer sink for every frame kind.
    let send_task = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let msg = match out {
                Out::Text(line) => Message::Text(line),
                Out::Binary(blob) => Message::Binary(blob),
                Out::Ping => Message::Ping(Vec::new()),
                Out::Close(frame) => {
                    // A client Close is ECHOED with its payload and FLUSHED
                    // before the socket goes: aborting here is what made the
                    // client see 1006 instead of the code it sent.
                    let _ = sink.send(Message::Close(frame)).await;
                    let _ = sink.flush().await;
                    let _ = sink.close().await;
                    break;
                }
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // THE CONNECT-TIME AUTO-WATCH of the endpoint the socket named — through the
    // one watch path, so a refusal is an `error` frame, never silence.
    if admits_autowatch {
        watch_path(
            &state.kernel,
            &leg_agent_id,
            &host_agent_id,
            &client_id,
            &mut watching,
            &readers,
            &out_tx,
            &host_agent_id.0,
            &json!(""),
        )
        .await;
    }

    // THE PEER HALF — the page kernel's address on the host, once it says hello.
    let mut peer = peer::PeerLink::new(&state.kernel, client_id.0.clone());

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick completes immediately; skip it
    let mut awaiting_pong: Option<tokio::time::Instant> = None;

    loop {
        let msg = tokio::select! {
            msg = stream.next() => msg,
            _ = ping.tick() => {
                if awaiting_pong.is_some_and(|sent| sent.elapsed() >= PING_TIMEOUT) {
                    let _ = out_tx
                        .send(Out::Close(Some(CloseFrame {
                            code: CloseCode::from(CLOSE_INTERNAL_ERROR),
                            reason: KEEPALIVE_TIMEOUT_REASON.into(),
                        })))
                        .await;
                    break;
                }
                awaiting_pong = Some(tokio::time::Instant::now());
                if out_tx.send(Out::Ping).await.is_err() {
                    break;
                }
                continue;
            }
        };
        let Some(msg) = msg else { break };
        let raw = match msg {
            Ok(Message::Text(t)) => InboundRaw::Text(t),
            Ok(Message::Binary(b)) => InboundRaw::Binary(b),
            Ok(Message::Pong(_)) => {
                awaiting_pong = None;
                continue;
            }
            Ok(Message::Close(frame)) => {
                let _ = out_tx.send(Out::Close(frame)).await;
                break;
            }
            Err(_) => break,
            _ => continue,
        };
        // A FRAME IS AN INPUT, NOT A CONTRACT. A 3-byte binary frame, a text
        // frame that parses to `[1]`, a `_binary_path` naming a slot that is not
        // there — each is answered and the SOCKET IS KEPT, with every in-flight
        // call on it.
        let Ok(inbound) = decode_inbound(raw) else {
            let _ = out_tx.send(Out::Text(encode_frame(&frame_error()))).await;
            continue;
        };
        let frame = inbound.envelope;
        let ty = frame.get("type").and_then(Value::as_str).unwrap_or("");
        // python `frame.get("target", "")` — the URL host is NEVER the default.
        let target = AgentId::from(
            frame
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        // python `frame.get("payload", {})`.
        let payload = match frame.get("payload") {
            Some(v) => v.clone(),
            None => json!({}),
        };
        // python `frame.get("id", "")` — kept RAW (any JSON type) and always
        // echoed; only a non-empty STRING names the execution.
        let fid = frame.get("id").cloned().unwrap_or_else(|| json!(""));
        let kernel = Arc::clone(&state.kernel);
        let leg = leg_agent_id.clone();
        let out = out_tx.clone();
        match ty {
            "call" => {
                let denial = match gate_leg(&kernel, &leg, "call", &frame) {
                    Decision::Allow => None,
                    // TEACHING DENIAL: the caller's promise resolves with the
                    // reason plus the pointer to the readme that says how to
                    // open the edge.
                    Decision::Deny(d) => Some(d.to_refusal()),
                };
                if let Some(data) = denial {
                    let f = json!({"type": "reply", "id": fid, "data": data});
                    let _ = out.send(Out::Text(encode_frame(&f))).await;
                    peer::mirror_reply(peer.mirror().siblings(), &fid, &data, None).await;
                    continue;
                }
                let body = inbound.body;
                // SECRET MARKS cross the leg verbatim (`fantastic_kernel::secret`):
                // this kernel records the call as shown, exactly as for a local
                // caller — the kernel's own check refuses a malformed mark before
                // anything is dispatched.
                let secret = frame.get("secret").cloned();
                // REGISTERED UNDER THE HOST: a frame handler DISPATCHES, so it
                // is real work — a `ps` row, reachable by `cancel`, and its send
                // parented to it instead of becoming a root.
                let spawner = Arc::clone(&state.kernel);
                let flush_readers = Arc::clone(&readers);
                let mirror = peer.mirror();
                let handle = spawner.spawn_task(&host_agent_id, "ws_call", async move {
                    // THE FRAME ID IS THE EXECUTION ID. One id space: the same
                    // id arriving twice — a double-click, a second tab, a
                    // reconnect replaying its queue — is ONE intent, deduped
                    // and joined by the ledger instead of running twice.
                    let mut item = Item::new(target, payload);
                    if let Some(s) = fid.as_str().filter(|s| !s.is_empty()) {
                        item = item.with_id(s);
                    }
                    if let Some(marks) = secret {
                        item = item.with_secret(marks);
                    }
                    // ONLY A BINARY FRAME OVERWRITES THE KEY. python's
                    // `payload[field] = body` runs where a body EXISTS; a TEXT
                    // frame's `bytes` reaches the target as the JSON value the
                    // client sent, and `write_stream` answers `missing_arg` for
                    // it (an ABSENT key stays an empty chunk). Stripping it
                    // unconditionally wrote 0 bytes and reported success where
                    // python refused. The decode left `null` in the slot
                    // python's `set_path` fills — kept, so the call row and the
                    // watcher frame keep python's key order.
                    //
                    // The REPLY needs nothing from the call: any verb whose reply
                    // carries a body answers a binary frame (`send_envelope`),
                    // whatever frame asked — python's `encode_frame` finds the
                    // bytes, no verb list.
                    if let Some(b) = body {
                        item = item.with_binary(b.path, b.bytes);
                    }
                    let out_item = fantastic_kernel::send::with_sender(leg, async {
                        kernel.send(vec![item]).await.remove(0)
                    })
                    .await;
                    // Verb-level errors travel inside `data.error` (python's
                    // wire shape); a `type:"error"` frame is reserved for
                    // out-of-band failures.
                    // PYTHON'S ORDER, ENFORCED: the watch events this dispatch
                    // produced are already on the queue (the kernel fans out
                    // BEFORE the handler), so flush them here — under the drain's
                    // own lock, so the drain cannot be mid-forward — and only then
                    // enqueue the reply. One sink, one order.
                    let snapshot: Vec<Reader> =
                        flush_readers.lock().expect("readers poisoned").clone();
                    let mut held = Vec::with_capacity(snapshot.len());
                    for (src, rx) in &snapshot {
                        let mut g = rx.lock().await;
                        while let Some(item) = g.try_recv() {
                            forward_event(&out, item, src.as_deref()).await;
                        }
                        held.push(g);
                    }
                    // the guid's other tabs get it too (`reply_mirror`) — read
                    // at reply time, so a hello that landed meanwhile counts
                    let siblings = mirror.siblings();
                    let mirrored = (!siblings.is_empty())
                        .then(|| (out_item.result.clone(), out_item.body.clone()));
                    let f = json!({"type": "reply", "id": fid, "data": out_item.result});
                    send_envelope(&out, f, out_item.body, REPLY_KEY).await;
                    drop(held);
                    if let Some((data, body)) = mirrored {
                        peer::mirror_reply(siblings, &fid, &data, body.as_ref()).await;
                    }
                });
                track_call(&state.kernel, &pending_calls, handle);
            }
            "hello" => peer.hello(&kernel, &leg, &out, &frame, &fid).await,
            // An answer to one of the host's calls INTO the page — never gated:
            // it only settles a call this leg itself sent.
            "reply" | "error" => peer.answer(&frame, inbound.answer_body),
            "emit" => {
                if !matches!(gate_leg(&kernel, &leg, "emit", &frame), Decision::Allow) {
                    continue;
                }
                // The body rides at ITS path; the decode already left `null` in
                // that slot of `payload`, in python's position.
                let item = match inbound.body {
                    None => fantastic_kernel::inbox::InboxItem::text(payload),
                    Some(b) => fantastic_kernel::inbox::InboxItem::binary(payload, b),
                };
                fantastic_kernel::send::with_sender(leg, async {
                    kernel.emit_item(&target, item).await;
                })
                .await;
            }
            "watch" => {
                if !matches!(gate_leg(&kernel, &leg, "watch", &frame), Decision::Allow) {
                    continue;
                }
                let src = frame.get("src").and_then(Value::as_str).unwrap_or("");
                watch_path(
                    &kernel,
                    &leg,
                    &host_agent_id,
                    &client_id,
                    &mut watching,
                    &readers,
                    &out,
                    src,
                    &fid,
                )
                .await;
            }
            "unwatch" => {
                // NO GATE — python gates the four inbound actions and leaves
                // `unwatch` (a client giving something up) ungated.
                let src = frame.get("src").and_then(Value::as_str).unwrap_or("");
                if let Some(entry) = watching.remove(src) {
                    drop_watch(&kernel, src, entry, &readers).await;
                }
            }
            "state_subscribe" => {
                // A SEALED LEG LEAKS NO TELEMETRY, not merely no dispatch.
                if !matches!(
                    gate_leg(&kernel, &leg, "state_subscribe", &frame),
                    Decision::Allow
                ) {
                    continue;
                }
                // The state stream is a KERNEL-WIDE read: every send's sender,
                // target, verb and payload summary, for every agent in the
                // tree. So it is gated on the ROOT — an ingress rule answers
                // "may this client reach this leg", never "what may it READ".
                let denial = fantastic_kernel::send::with_sender(leg.clone(), async {
                    kernel.authorize_state_subscribe(&leg)
                })
                .await;
                if let Some(d) = denial {
                    let _ = out
                        .send(Out::Text(encode_frame(&error_frame(&fid, &d))))
                        .await;
                    continue;
                }
                // python parity: first a `state_snapshot` carrying every
                // agent's identity so a state-watching view can bootstrap
                // before the first event, THEN the `state_event` stream.
                let snapshot = json!({"type": "state_snapshot", "agents": kernel.state_snapshot()});
                let _ = out_tx.send(Out::Text(encode_frame(&snapshot))).await;
                let out_for_cb = out_tx.clone();
                let kernel_for_cb = Arc::clone(&kernel);
                // Conversation rows are not on this stream — an AI agent
                // announces its own rows to its watchers (`watch` on that agent,
                // gated there).
                let token = kernel.add_state_subscriber(Arc::new(
                    move |event: &Value, body: Option<&fantastic_kernel::Blob>| {
                        let Value::Object(ev) = event else {
                            return;
                        };
                        // `type` FIRST, then the event's own keys — python's
                        // `{"type": "state_event", **event}`, which is the byte
                        // order a frame diff reads.
                        let mut frame = serde_json::Map::new();
                        frame.insert("type".to_string(), Value::String("state_event".into()));
                        for (k, v) in ev {
                            frame.insert(k.clone(), v.clone());
                        }
                        // Lazy `name` fill — python derives it from the agent
                        // record when the event lacks one, so the browser always
                        // renders one shape. An agent already gone falls back to
                        // its id rather than dropping the key. Python's
                        // `rec.get("display_name") or agent_id`: the RAW value,
                        // by truthiness.
                        if !frame.contains_key("name") {
                            if let Some(aid) = frame.get("agent_id").and_then(Value::as_str) {
                                let name = kernel_for_cb
                                    .agents
                                    .get(&AgentId::from(aid))
                                    .and_then(|a| {
                                        a.meta
                                            .read()
                                            .expect("meta lock poisoned")
                                            .get("display_name")
                                            .filter(|v| fantastic_bundle::py_truthy(v))
                                            .cloned()
                                    })
                                    .unwrap_or_else(|| Value::String(aid.to_string()));
                                frame.insert("name".into(), name);
                            }
                        }
                        let frame = Value::Object(frame);
                        // Best-effort: drop if the drain channel is gone
                        // (disconnected mid-fanout).
                        let out = match body {
                            None => Out::Text(encode_frame(&frame)),
                            Some(b) => Out::Binary(binary_out(frame, b, INBOUND_PAYLOAD_KEY)),
                        };
                        let _ = out_for_cb.try_send(out);
                    },
                ));
                state_unsubs
                    .lock()
                    .expect("state_unsubs poisoned")
                    .push(token);
            }
            "state_unsubscribe" => {
                let tokens: Vec<_> = state_unsubs
                    .lock()
                    .expect("state_unsubs poisoned")
                    .drain(..)
                    .collect();
                for t in tokens {
                    kernel.remove_state_subscriber(t);
                }
            }
            // Unknown frame types are silently dropped (forward-compat).
            _ => {}
        }
    }

    // ─── Cleanup on disconnect (python's `finally` block) ──────────
    // 0. The page is gone: drop its address and settle every host call still
    //    waiting on it (python does this first, before anything is cancelled).
    peer.close(&state.kernel);
    // 1. Cancel any in-flight `call` handlers so kernel.send tasks unwind.
    {
        let ids: Vec<_> = {
            let mut p = pending_calls.lock().expect("pending poisoned");
            p.drain(..).collect()
        };
        for t in ids {
            // Through the TABLE — `on_cancel` runs, as for any cancel.
            state.kernel.cancel_task(&t).await;
        }
    }
    // 2. Unwatch every src this WS subscribed to.
    {
        let all: Vec<(String, WatchEntry)> = watching.drain().collect();
        for (src, entry) in all {
            drop_watch(&state.kernel, &src, entry, &readers).await;
        }
    }
    // 3. Unregister any state-stream callbacks tied to this WS.
    {
        let tokens: Vec<_> = state_unsubs
            .lock()
            .expect("state_unsubs poisoned")
            .drain(..)
            .collect();
        for t in tokens {
            state.kernel.remove_state_subscriber(t);
        }
    }
    // 4. Drop the synthetic inbox, then let the sink drain its queued close.
    state.kernel.remove_inbox(&client_id);
    state.kernel.cancel_task(&drain_task).await;
    drop(out_tx);
    let _ = send_task.await;
}

/// THE ONE WATCH PATH — the connect-time auto-watch and an explicit `watch`
/// frame both come here. Python `web_ws._proxy._watch`.
///
/// FAIL LOUD: a name that resolves to no agent, or a gate refusal, is answered
/// with an `error` frame and NOT recorded as watched. (The connect-time watch
/// used to discard the result and mark the name watched anyway; a name the
/// kernel's `watch` did not resolve mirrored nothing, said nothing, and the
/// explicit watch that followed was then skipped as a duplicate.)
///
/// The name resolves the way `send` resolves a target (`kernel` or a literal
/// id), and the watch runs UNDER THE LEG'S OWN IDENTITY, so
/// the kernel's gate sees a real caller — without it the kernel reads the watch
/// as an OPERATOR's and the two-sided gate never runs.
#[allow(clippy::too_many_arguments)]
async fn watch_path(
    kernel: &Arc<fantastic_kernel::Kernel>,
    leg: &AgentId,
    host: &AgentId,
    client_id: &AgentId,
    watching: &mut std::collections::HashMap<String, WatchEntry>,
    readers: &Readers,
    out: &tokio::sync::mpsc::Sender<Out>,
    src: &str,
    fid: &Value,
) {
    if src.is_empty() || watching.contains_key(src) {
        return;
    }
    let Some(target) = kernel.resolve_target(src) else {
        let mut r = fantastic_kernel::refusal(
            fantastic_kernel::refusal::UNKNOWN_AGENT,
            format!(
                "watch: no agent {} (an id, or `kernel`)",
                fantastic_bundle::py_repr(src)
            ),
        );
        fantastic_kernel::refusal::with(&mut r, "src", json!(src));
        let _ = out
            .send(Out::Text(encode_frame(&error_frame(fid, &r))))
            .await;
        return;
    };
    // THIS WATCH'S OWN READER — registered before the watch asks for one.
    let reader = AgentId::from(format!("{client_id}>{}", target.id).as_str());
    let rx = Arc::new(tokio::sync::Mutex::new(kernel.ensure_inbox(&reader)));
    let ready = kernel.ensure_inbox(&reader);
    let denial =
        fantastic_kernel::send::with_sender(leg.clone(), kernel.watch(&target.id, reader.clone()))
            .await;
    if let Some(d) = denial {
        kernel.remove_inbox(&reader);
        // python `{"type":"error", "id":…, **denial}` — the denial's keys are
        // FLATTENED onto the frame, not wrapped in `data`.
        let _ = out
            .send(Out::Text(encode_frame(&error_frame(fid, &d))))
            .await;
        return;
    }
    let tag = src.to_string();
    let task = Arc::clone(kernel).spawn_task(host, "ws_drain", {
        let out = out.clone();
        let rx = Arc::clone(&rx);
        async move {
            loop {
                {
                    let mut g = rx.lock().await;
                    while let Some(item) = g.try_recv() {
                        if !forward_event(&out, item, Some(&tag)).await {
                            return;
                        }
                    }
                }
                ready.ready().await;
            }
        }
    });
    readers
        .lock()
        .expect("readers poisoned")
        .push((Some(src.to_string()), rx));
    watching.insert(
        src.to_string(),
        WatchEntry {
            target: target.id.clone(),
            reader,
            task,
        },
    );
}

/// One registered watch — python's `watching[src] = (aid, reader, drain)`.
pub(crate) struct WatchEntry {
    target: AgentId,
    reader: AgentId,
    task: fantastic_kernel::TaskId,
}

/// A reader this socket drains: the `src` its events are stamped with (`None` for
/// the connection's own inbox) and the locked receive half.
type Reader = (
    Option<String>,
    Arc<tokio::sync::Mutex<fantastic_kernel::inbox::InboxReceiver>>,
);
type Readers = Arc<std::sync::Mutex<Vec<Reader>>>;

/// Give a watch back: unwatch, stop its drain, DROP ITS INBOX, forget the reader.
async fn drop_watch(
    kernel: &Arc<fantastic_kernel::Kernel>,
    src: &str,
    entry: WatchEntry,
    readers: &Readers,
) {
    kernel.unwatch(&entry.target, &entry.reader).await;
    kernel.cancel_task(&entry.task).await;
    kernel.remove_inbox(&entry.reader);
    readers
        .lock()
        .expect("readers poisoned")
        .retain(|(s, _)| s.as_deref() != Some(src));
}

/// python `{"type": "error", "id": <id or "">, **denial}` — the denial's own
/// keys are flattened onto the frame.
fn error_frame(fid: &Value, denial: &Value) -> Value {
    let mut out = Map::new();
    out.insert("type".to_string(), json!("error"));
    out.insert("id".to_string(), fid.clone());
    if let Value::Object(m) = denial {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}
