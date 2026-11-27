//! The WebSocket frame loop — the rust port of python's
//! `PB/io/web_ws/src/web_ws/_proxy.py::run`.
//!
//! ```text
//! C→S {type:"call",  target, payload, id, secret?}
//!                                             → handed to the leg → reply (the ACK)
//!                                               then the settle event (#795)
//!                                               (`secret` = the item's marks —
//!                                               dotted strings and/or arrays
//!                                               of steps, `fantastic_kernel::secret`)
//! C→S {type:"emit",  target, payload}         → kernel.emit (no reply)
//! C→S {type:"watch"|"unwatch", src}           → (un)register a watcher
//! C→S {type:"state_subscribe", src}         → watch the state stream of `src`'s
//!                                               subtree, cut to what the leg covers
//! C→S {type:"state_unsubscribe"}
//! C→S {type:"hello", guid, leg?}              → the page kernel (or, with `leg`,
//!                                               a kernel's dialing leg) becomes
//!                                               the host's peer `<guid>|<agent>`
//! S→C {type:"watch"|"unwatch", src}            → the host watching a kernel
//!                                               leg's agent `src`
//! C→S {type:"event", src, payload}            → an event of that agent, to the
//!                                               host's watchers of `<guid>|<src>`
//! S→C {type:"reply", id, data} | {type:"error", id, …}
//!                                             (`data` = the call's ACK `{pid, status:
//!                                               "accepted"}`, or its refusal at launch,
//!                                               which is the whole answer)
//! S→C {type:"event", payload:{type:"settle", re, status, data, secret?}}
//!                                             → the call's SETTLE (#795), after its
//!                                               ack: `re` = the call frame's id,
//!                                               `data` its result (`secret` = dotted
//!                                               paths into `data` the reply's bundle
//!                                               marked — absent when none)
//! S→C {type:"event", src, payload}            → an event from a watched agent
//!                                               (`src` = the name the client watched)
//! S→C {type:"event", payload}                 → an event emitted to THIS connection
//! S→C {type:"call", id:"h-<nonce>-<n>", target, payload, caller, secret?}
//!                                             → the host calling a page agent
//! C→S {type:"reply"|"error", id:"h-<nonce>-<n>", data, secret?}
//!                                             → that call's outcome (the nonce is
//!                                               the connection's)
//! S→C {type:"emit", target, payload, caller?}  → a host emit to a page agent
//!                                               (every tab of the guid)
//! S→C {type:"reply_mirror", id, data}          → a reply this leg sent ANOTHER
//!                                               tab of this connection's guid
//! ```
//!
//! **CROSS-KERNEL ADDRESSING.** After `hello`, the host
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
//! wire exactly as python sends it.
//!
//! **IDENTITY.** Every frame runs as the web_ws LEG (`leg_agent_id`), the
//! child agent whose record carries the `ingress_rule` this face is gated by —
//! never as the URL host and never as the operator, or a browser would inherit
//! the origin root of trust. The two tasks the loop registers still belong to
//! the HOST (`ws_drain`, `ws_call`), which is what python's
//! `spawn_task(host_id, …)` does.
//!
//! **THE LEG GATE.** Python resolves the leg's ingress rule PER FRAME, so
//! opening the edge (`update_agent ingress_rule=allow_all_ingress`) takes effect
//! without a reconnect, and an ABSENT rule is SEALED. A denied `call` is
//! answered (`{error, reason:'unauthorized', hint?, see?}` inside the reply's
//! `data`) because discovery-through-denial is the point; every other kind is
//! dropped silently, the connect-time auto-watch included — a sealed leg that
//! auto-watched would leak the host's whole event stream to anyone who completed
//! the handshake.

use crate::AppState;
use fantastic_io_bridge::authorizer::{ingress, Action, Decision, Rules};
use fantastic_io_bridge::codec::{decode_binary_frame, encode_frame};
use fantastic_io_bridge::ws_deflate::DeflateStream;
use fantastic_kernel::{AgentId, Kernel};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
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

/// The fields of a frame's envelope the gate and the frame arms read, decoded
/// once; a frame whose envelope does not decode is a frame error.
#[derive(Deserialize)]
pub(crate) struct Envelope {
    #[serde(rename = "type")]
    ty: String,
    target: Option<String>,
    src: Option<String>,
    id: Option<Value>,
    secret: Option<Value>,
    auth_token: Option<String>,
    payload: Option<Value>,
}

impl Envelope {
    /// The envelope of a watch of `src`, as the connect-time auto-watch judges it.
    fn watch_of(src: &str) -> Envelope {
        Envelope {
            ty: "watch".to_string(),
            target: None,
            src: Some(src.to_string()),
            id: None,
            secret: None,
            auth_token: None,
            payload: None,
        }
    }
}

/// The verb of a call's payload: its `type`.
#[derive(Deserialize)]
struct Verbed {
    #[serde(rename = "type")]
    verb: Option<String>,
}

/// python `_gate` — the leg's INGRESS rule, resolved off the LEG's record on
/// every frame. A missing record, or a record with no rule, is SEALED.
///
/// `kind` ∈ call|emit|watch|state_subscribe; for `call` the verb is the
/// payload's `type`, otherwise the verb IS the kind (python `gate_inbound`).
/// The credential rides the frame ENVELOPE (`auth_token`), never the payload.
fn gate_leg(kernel: &Arc<Kernel>, leg: &AgentId, kind: &str, env: &Envelope) -> Decision {
    let rules = match kernel.resolve_target(leg.as_str()) {
        Some(a) => Rules::of(&a.record().meta).unwrap_or_default(),
        None => Rules::default(),
    };
    let verb = if kind == "call" {
        env.payload
            .as_ref()
            .and_then(|p| Verbed::deserialize(p).ok())
            .and_then(|v| v.verb)
            .unwrap_or_default()
    } else {
        kind.to_string()
    };
    // python `frame.get("target") or frame.get("src") or ""` — a falsy target
    // falls through to `src`, which is what a watch frame carries.
    let addr = env
        .target
        .as_deref()
        .filter(|t| !t.is_empty())
        .or(env.src.as_deref())
        .unwrap_or("");
    ingress::resolve(&rules).authorize(&Action {
        kind,
        target: addr,
        verb: &verb,
        token: env.auth_token.as_deref(),
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
        // STRICT: python `decode_frame` → `io_bridge.loads`.
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
    fantastic_kernel::lifecycle::fill_random(&mut buf);
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
    // reply could overtake the event it must follow. The lock below makes
    // flush-then-reply ONE step: every payload queued when the handler settled
    // reaches `out_tx` before the reply does, with no sleep and no yield.
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
            &Envelope::watch_of(&host_agent_id.0),
        ),
        Decision::Allow
    );

    // THE CONNECTION, shared with the steps and the continuations its frames make
    // (#795): nothing here is a local of the loop, because a settle arrives as a
    // message after the frame that asked was handled.
    let conn = Arc::new(Conn {
        kernel: Arc::clone(&state.kernel),
        leg: leg_agent_id.clone(),
        host: host_agent_id.clone(),
        client_id: client_id.clone(),
        out: out_tx.downgrade(),
        readers: Arc::clone(&readers),
        watching: std::sync::Mutex::new(Watches::default()),
        pending_calls: std::sync::Mutex::new(Vec::new()),
        state_subs: std::sync::Mutex::new(StateSubs::default()),
    });
    let order = Order::new();

    // REGISTERED, under the HOST: python spawns the drain as the kernel task
    // `ws_drain` on `host_agent_id`, so an open socket is visible in `ps` and
    // its pump is cancellable like any other work. A KEEPALIVE loop: the
    // operator's stop cancels the calls in flight, and their settles still reach
    // this socket (#795) — the pump ends with the socket, not with the drain.
    let drain_task = state
        .kernel
        .clone()
        .spawn_keepalive_task(&host_agent_id, "ws_drain", {
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
    let send_task = fantastic_kernel::spawn_inherit(async move {
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

    // THE LEG SAYS A SOCKET OPENED, on its own inbox: whoever watches the leg (a
    // program on a box a master dials) learns its link is up without asking.
    state
        .kernel
        .emit(&leg_agent_id, json!({"type": "socket_open", "client_id": client_id.0}))
        .await;

    // THE CONNECT-TIME AUTO-WATCH of the endpoint the socket named — through the
    // one watch path, so a refusal is an `error` frame, never silence. The first
    // step of the connection's order: every frame after it runs once it is live.
    if admits_autowatch {
        let (c, src) = (Arc::clone(&conn), host_agent_id.0.clone());
        order.push(Box::new(move |next| Box::pin(c.watch(src, json!(""), next))));
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
        let Ok(mut env) = Envelope::deserialize(&frame) else {
            let _ = out_tx.send(Out::Text(encode_frame(&frame_error()))).await;
            continue;
        };
        // python `frame.get("target", "")` — the URL host is NEVER the default.
        let target = AgentId::from(env.target.as_deref().unwrap_or_default());
        // python `frame.get("id", "")` — kept RAW (any JSON type) and always
        // echoed; only a non-empty STRING names the execution.
        let fid = env.id.take().unwrap_or_else(|| json!(""));
        let secret = env.secret.take();
        let kernel = Arc::clone(&state.kernel);
        let leg = leg_agent_id.clone();
        let out = out_tx.clone();
        match env.ty.as_str() {
            "call" => {
                let denial = match gate_leg(&kernel, &leg, "call", &env) {
                    Decision::Allow => None,
                    // TEACHING DENIAL: the caller's promise resolves with the
                    // reason plus the pointer that says how to
                    // open the edge.
                    Decision::Deny(d) => Some(d.to_refusal()),
                };
                if let Some(data) = denial {
                    let f = json!({"type": "reply", "id": fid, "data": data});
                    let _ = out.send(Out::Text(encode_frame(&f))).await;
                    peer::mirror_reply(peer.mirror().siblings(), &fid, &data, None).await;
                    continue;
                }
                // the marks are a list of paths: anything else is refused here,
                // before the call runs
                if secret.as_ref().is_some_and(|m| !m.is_array()) {
                    let data = fantastic_kernel::refusal::bad("secret", "secret", "a list of paths");
                    let f = json!({"type": "reply", "id": fid, "data": data});
                    let _ = out.send(Out::Text(encode_frame(&f))).await;
                    peer::mirror_reply(peer.mirror().siblings(), &fid, &data, None).await;
                    continue;
                }
                // python `frame.get("payload", {})`.
                let payload = env.payload.take().unwrap_or_else(|| json!({}));
                // THE FRAME ID IS THE EXECUTION ID. One id space: the same
                // id arriving twice — a double-click, a second tab, a
                // reconnect replaying its queue — is ONE intent, deduped
                // and joined by the ledger instead of running twice.
                let mut item = serde_json::Map::new();
                item.insert("target".into(), json!(target.as_str()));
                item.insert("payload".into(), payload);
                if let Some(s) = fid.as_str().filter(|s| !s.is_empty()) {
                    item.insert("id".into(), json!(s));
                }
                // paths into `payload`, shown as `[secret]`
                if let Some(m) = secret {
                    item.insert("secret".into(), m);
                }
                // ONLY A BINARY FRAME OVERWRITES THE KEY. python's
                // `payload[field] = body` runs where a body EXISTS; a TEXT
                // frame's `bytes` reaches the target as the JSON value the
                // client sent, and `write_stream` answers `missing_arg` for
                // it (an ABSENT key stays an empty chunk). The decode left
                // `null` in the slot python's `set_path` fills — kept, so the
                // call row and the watcher frame keep python's key order.
                //
                // The SETTLE needs nothing from the call: any verb whose reply
                // carries a body answers a binary frame (`send_envelope`),
                // whatever frame asked.
                let body = inbound.body.map(|b| {
                    let mut path = vec![fantastic_kernel::Step::Key("item".into())];
                    path.extend(b.nested_in("payload"));
                    fantastic_kernel::Blob::new(path, b.bytes)
                });
                let (c, mirror) = (Arc::clone(&conn), peer.mirror());
                let hand = json!({"type": "inbound_call", "item": Value::Object(item)});
                order.push(Box::new(move |next| Box::pin(c.call(fid, hand, body, mirror, next))));
            }
            "hello" => peer.hello(&kernel, &leg, &out, &frame, &env, &fid).await,
            // An answer to one of the host's calls INTO the page — never gated:
            // it only settles a call this leg itself sent.
            "reply" | "error" => peer.answer(&kernel, &frame, inbound.answer_body),
            // An event a kernel's leg streams for an agent the host watches —
            // never gated: it only answers a watch this leg itself sent.
            "event" => peer.event(&kernel, &frame, inbound.body),
            "emit" => {
                if !matches!(gate_leg(&kernel, &leg, "emit", &env), Decision::Allow) {
                    continue;
                }
                // python `frame.get("payload", {})`.
                let payload = env.payload.take().unwrap_or_else(|| json!({}));
                // HANDED TO THE LEG (`inbound_emit`), which emits AS ITSELF. The
                // body rides at ITS path below `payload`; the decode already left
                // `null` in that slot, in python's position.
                let body = inbound
                    .body
                    .map(|b| fantastic_kernel::Blob::new(b.nested_in("payload"), b.bytes));
                let c = Arc::clone(&conn);
                let hand = json!({"type": "inbound_emit", "target": target.as_str(), "payload": payload});
                order.push(Box::new(move |next| Box::pin(c.emit(hand, body, next))));
            }
            "watch" => {
                if !matches!(gate_leg(&kernel, &leg, "watch", &env), Decision::Allow) {
                    continue;
                }
                let (c, src) = (Arc::clone(&conn), env.src.clone().unwrap_or_default());
                order.push(Box::new(move |next| Box::pin(c.watch(src, fid, next))));
            }
            "unwatch" => {
                // NO GATE — python gates the four inbound actions and leaves
                // `unwatch` (a client giving something up) ungated.
                let (c, src) = (Arc::clone(&conn), env.src.clone().unwrap_or_default());
                order.push(Box::new(move |next| Box::pin(c.unwatch(src, next))));
            }
            "state_subscribe" => {
                // A SEALED LEG LEAKS NO TELEMETRY, not merely no dispatch.
                if !matches!(
                    gate_leg(&kernel, &leg, "state_subscribe", &env),
                    Decision::Allow
                ) {
                    continue;
                }
                let (c, node) = (Arc::clone(&conn), AgentId::from(env.src.as_deref().unwrap_or("")));
                order.push(Box::new(move |next| Box::pin(c.state_subscribe(node, fid, next))));
            }
            "state_unsubscribe" => {
                let c = Arc::clone(&conn);
                order.push(Box::new(move |next| Box::pin(c.state_unsubscribe(next))));
            }
            // Unknown frame types are silently dropped (forward-compat).
            _ => {}
        }
    }

    // ─── Cleanup on disconnect (python's `finally` block) ──────────
    // 0. The page is gone: drop its address and settle every host call still
    //    waiting on it (python does this first, before anything is cancelled).
    let peer_guid = peer.guid();
    peer.close(&state.kernel);
    // 1. Cancel any in-flight `call` so its execution unwinds; its settle then
    //    finds no socket to tell.
    {
        let ids: Vec<_> = {
            let mut p = conn.pending_calls.lock().expect("pending poisoned");
            p.drain(..).collect()
        };
        for t in ids {
            // Through the TABLE — `on_cancel` runs, as for any cancel.
            state.kernel.cancel_task(&t).await;
        }
    }
    // 2. Unwatch every src this WS subscribed to; a watch still on its way finds
    //    the connection closed and gives itself back.
    {
        let all: Vec<(String, WatchEntry)> = {
            let mut w = conn.watching.lock().expect("watching poisoned");
            w.closed = true;
            w.live.drain().collect()
        };
        for (src, entry) in all {
            drop_watch(&state.kernel, &src, entry, &readers).await;
        }
    }
    // 3. Unregister any state-stream callbacks tied to this WS.
    {
        let tokens: Vec<_> = {
            let mut s = conn.state_subs.lock().expect("state_subs poisoned");
            s.closed = true;
            std::mem::take(&mut s.tokens)
        };
        for t in tokens {
            state.kernel.remove_state_subscriber(t);
        }
    }
    // 4. Drop the synthetic inbox, then let the sink drain its queued close. The
    //    connection holds only a WEAK sender, so a settle still on its way cannot
    //    keep the socket open.
    state.kernel.remove_inbox(&client_id);
    state.kernel.cancel_task(&drain_task).await;
    drop(out_tx);
    let _ = send_task.await;
    // 5. THE LEG SAYS THE SOCKET CLOSED, after its calls settled: a closed peer,
    //    a dead tunnel or a keepalive timeout alike. The side that was dialed
    //    cannot dial back; its watchers wait for the next `socket_open`.
    state
        .kernel
        .emit(
            &leg_agent_id,
            json!({"type": "socket_closed", "client_id": client_id.0, "guid": peer_guid}),
        )
        .await;
}

/// One step of a connection's ORDER: it gets the [`Next`] handle, and the next
/// step starts when that handle is dropped.
type Step = Box<
    dyn FnOnce(Next) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send,
>;

/// THE CONNECTION'S ORDER (#795). The frames of one connection that the leg
/// handles take effect IN THE ORDER THEY CAME, as when each was handled inline: a
/// `watch` is live before the frame after it runs, an `emit` is emitted, a `call`
/// is launched (its ACK; its settle comes whenever it comes). Nothing waits for
/// it: a step holds the [`Next`] handle until its effect is done — a `call` until
/// its ack, a `watch`/`emit`/`state_subscribe` until the leg's settle, in the
/// continuation — and dropping it starts the next step. The read loop never
/// blocks on a step; the frames queue here.
#[derive(Clone)]
struct Order(Arc<std::sync::Mutex<Queue>>);

#[derive(Default)]
struct Queue {
    busy: bool,
    steps: std::collections::VecDeque<Step>,
}

/// The right to start the next step of an [`Order`]: dropped when this step's
/// effect is done (or the step ends without one).
struct Next(Order);

impl Order {
    fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(Queue::default())))
    }

    /// Queue `step`; it starts at once when nothing runs.
    fn push(&self, step: Step) {
        let run = {
            let mut q = self.0.lock().expect("order poisoned");
            if q.busy {
                q.steps.push_back(step);
                None
            } else {
                q.busy = true;
                Some(step)
            }
        };
        if let Some(s) = run {
            self.start(s);
        }
    }

    /// Run one step as its own task, with this connection's identity (the
    /// transport's: the operator).
    fn start(&self, step: Step) {
        fantastic_kernel::spawn_inherit(step(Next(self.clone())));
    }
}

impl Drop for Next {
    fn drop(&mut self) {
        let next = {
            let mut q = self.0 .0.lock().expect("order poisoned");
            let next = q.steps.pop_front();
            if next.is_none() {
                q.busy = false;
            }
            next
        };
        if let Some(s) = next {
            self.0.start(s);
        }
    }
}

/// The watches of one connection: the live ones by the name the client used, and
/// whether the socket closed (a watch that lands after that gives itself back).
#[derive(Default)]
struct Watches {
    closed: bool,
    live: std::collections::HashMap<String, WatchEntry>,
}

/// The state-stream subscriptions of one connection, and whether it closed.
#[derive(Default)]
struct StateSubs {
    closed: bool,
    tokens: Vec<fantastic_kernel::SubscriberToken>,
}

/// ONE CONNECTION, as its steps and continuations see it.
struct Conn {
    kernel: Arc<Kernel>,
    /// THIS web_ws surface: the identity every frame is handed in as.
    leg: AgentId,
    /// The endpoint the socket named; the drains are its tasks.
    host: AgentId,
    client_id: AgentId,
    /// WEAK: a settle that lands after the socket closed has nowhere to go and
    /// keeps nothing open.
    out: tokio::sync::mpsc::WeakSender<Out>,
    readers: Readers,
    watching: std::sync::Mutex<Watches>,
    /// The executions of this connection's calls not yet settled (their ack pids)
    /// — cancelled on disconnect.
    pending_calls: std::sync::Mutex<Vec<fantastic_kernel::TaskId>>,
    state_subs: std::sync::Mutex<StateSubs>,
}

/// Has a `call`'s ACK gone out? A settle that comes first waits here, so the
/// client always reads the ack before the settle.
#[derive(Default)]
struct AckGate {
    sent: bool,
    early: Option<fantastic_kernel::Outcome>,
}

impl Conn {
    fn out(&self) -> Option<tokio::sync::mpsc::Sender<Out>> {
        self.out.upgrade()
    }

    async fn send_text(&self, frame: &Value) {
        if let Some(o) = self.out() {
            let _ = o.send(Out::Text(encode_frame(frame))).await;
        }
    }

    /// A `call` frame (#795): handed to the leg, which sends it onward AS THE LEG
    /// (`fantastic_io_bridge::inbound`). The client gets the ACK at once as the
    /// `reply` frame (`data`: `{pid, status: "accepted"}`, or the refusal), and the
    /// result later as `{type: "event", payload: {type: "settle", re: <frame id>,
    /// status, data, secret?}}` on this connection.
    async fn call(
        self: Arc<Self>,
        fid: Value,
        hand: Value,
        body: Option<fantastic_kernel::Blob>,
        mirror: peer::Mirror,
        next: Next,
    ) {
        let gate = Arc::new(std::sync::Mutex::new(AckGate::default()));
        let (c, g, f, m) = (Arc::clone(&self), Arc::clone(&gate), fid.clone(), mirror.clone());
        let ack = fantastic_io_bridge::inbound::hand_in(&self.kernel, &self.leg, hand, body, move |out| async move {
            let early = {
                let mut g = g.lock().expect("ack gate poisoned");
                if g.sent {
                    Some(out)
                } else {
                    g.early = Some(out);
                    None
                }
            };
            if let Some(out) = early {
                c.settled(&f, out, &m).await;
            }
        })
        .await;
        let accepted = ack.status == fantastic_kernel::Status::Accepted;
        if let (true, Some(pid)) = (accepted, ack.pid) {
            track_call(&self.kernel, &self.pending_calls, pid);
        }
        let data = fantastic_io_bridge::inbound::ack_value(&ack);
        self.send_text(&json!({"type": "reply", "id": fid, "data": data})).await;
        // the launch is done: the next frame of this connection may run
        drop(next);
        if !accepted {
            // refused at launch: the refusal is the answer, and nothing settles
            // later; the guid's other tabs see it too
            peer::mirror_reply(mirror.siblings(), &fid, &data, None).await;
            return;
        }
        let early = {
            let mut g = gate.lock().expect("ack gate poisoned");
            g.sent = true;
            g.early.take()
        };
        if let Some(out) = early {
            self.settled(&fid, out, &mirror).await;
        }
    }

    /// A call's SETTLE, told to this connection — after every event the watches
    /// of this socket already queued (PYTHON'S ORDER: the watch events a dispatch
    /// produced are flushed under the drains' own locks, so no drain is
    /// mid-forward, and only then the settle goes out; one sink, one order); then
    /// the guid's other tabs get the result as `reply_mirror`.
    async fn settled(&self, fid: &Value, out: fantastic_kernel::Outcome, mirror: &peer::Mirror) {
        let Some(tx) = self.out() else { return };
        let snapshot: Vec<Reader> = self.readers.lock().expect("readers poisoned").clone();
        let mut held = Vec::with_capacity(snapshot.len());
        for (src, rx) in &snapshot {
            let mut g = rx.lock().await;
            while let Some(item) = g.try_recv() {
                forward_event(&tx, item, src.as_deref()).await;
            }
            held.push(g);
        }
        let siblings = mirror.siblings();
        let mirrored = (!siblings.is_empty()).then(|| (out.result.clone(), out.body.clone()));
        let mut payload = fantastic_io_bridge::inbound::settle_payload(fid.clone(), &out);
        // the reply's own secret paths (a credential store's `get` fields), so a
        // calling kernel records them as shown; a plain settle carries no such key
        if !out.secret.is_empty() {
            payload["secret"] = json!(out.secret);
        }
        let body = out
            .body
            .map(|b| fantastic_kernel::Blob::new(b.nested_in("data"), b.bytes));
        send_envelope(&tx, json!({"type": "event", "payload": payload}), body, INBOUND_PAYLOAD_KEY).await;
        drop(held);
        if let Some((data, body)) = mirrored {
            peer::mirror_reply(siblings, fid, &data, body.as_ref()).await;
        }
    }

    /// An `emit` frame: handed to the leg (`inbound_emit`), which emits AS ITSELF.
    /// The next frame runs once it is emitted.
    async fn emit(self: Arc<Self>, hand: Value, body: Option<fantastic_kernel::Blob>, next: Next) {
        let _ = fantastic_io_bridge::inbound::hand_in(&self.kernel, &self.leg, hand, body, move |_| async move {
            drop(next);
        })
        .await;
    }

    /// THE ONE WATCH PATH — the connect-time auto-watch and an explicit `watch`
    /// frame both come here. Python `web_ws._proxy._watch`.
    ///
    /// FAIL LOUD: a name that resolves to no agent, or a gate refusal, is answered
    /// with an `error` frame and NOT recorded as watched — a name the kernel's
    /// `watch` did not resolve would otherwise mirror nothing, say nothing, and skip
    /// the explicit watch that follows as a duplicate.
    ///
    /// The name resolves the way `send` resolves a target (`kernel` or a literal
    /// id), and the watch is HANDED TO THE LEG (`inbound_watch`), whose handler
    /// asks the kernel as the leg — so the two-sided gate judges the leg. Its
    /// settle (a message to this connection) makes the watch live, or answers its
    /// refusal; the next frame runs after that.
    async fn watch(self: Arc<Self>, src: String, fid: Value, next: Next) {
        if src.is_empty() || self.watching.lock().expect("watching poisoned").live.contains_key(&src) {
            return;
        }
        let Some(target) = self.kernel.resolve_target(&src) else {
            let mut r = fantastic_kernel::refusal(
                fantastic_kernel::refusal::UNKNOWN_AGENT,
                format!(
                    "watch: no agent {} (an id, or `kernel`)",
                    fantastic_bundle::py_repr(&src)
                ),
            );
            fantastic_kernel::refusal::with(&mut r, "src", json!(src));
            self.send_text(&error_frame(&fid, &r)).await;
            return;
        };
        // THIS WATCH'S OWN READER — registered before the watch asks for one.
        let reader = AgentId::from(format!("{}>{}", self.client_id, target.id).as_str());
        let rx = Arc::new(tokio::sync::Mutex::new(self.kernel.ensure_inbox(&reader)));
        let ready = self.kernel.ensure_inbox(&reader);
        let hand = json!({"type": "inbound_watch", "src": target.id.as_str(), "reader": reader.as_str()});
        let (c, r, s, f, t) = (Arc::clone(&self), reader.clone(), src.clone(), fid.clone(), target.id.clone());
        let ack = fantastic_io_bridge::inbound::hand_in(&self.kernel, &self.leg, hand, None, move |out| async move {
            c.watched(s, t, r, rx, ready, &f, &out.result).await;
            drop(next);
        })
        .await;
        if ack.status != fantastic_kernel::Status::Accepted {
            self.kernel.remove_inbox(&reader);
            self.send_text(&error_frame(&fid, &ack.result)).await;
        }
    }

    /// A watch's settle: live — its drain registered — or refused (an `error`
    /// frame, nothing recorded). One that lands after the socket closed is given
    /// back.
    #[allow(clippy::too_many_arguments)]
    async fn watched(
        &self,
        src: String,
        target: AgentId,
        reader: AgentId,
        rx: Arc<tokio::sync::Mutex<fantastic_kernel::inbox::InboxReceiver>>,
        ready: fantastic_kernel::inbox::InboxReceiver,
        fid: &Value,
        asked: &Value,
    ) {
        if fantastic_kernel::refusal::error_text(asked).is_some() {
            self.kernel.remove_inbox(&reader);
            // python `{"type":"error", "id":…, **denial}` — the denial's keys are
            // FLATTENED onto the frame, not wrapped in `data`.
            self.send_text(&error_frame(fid, asked)).await;
            return;
        }
        let Some(out) = self.out() else {
            self.kernel.unwatch(&target, &reader).await;
            self.kernel.remove_inbox(&reader);
            return;
        };
        let tag = src.clone();
        // a keepalive loop, as the connection's own drain: it ends with the socket
        let task = Arc::clone(&self.kernel).spawn_keepalive_task(&self.host, "ws_drain", {
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
        let entry = WatchEntry {
            target,
            reader,
            task,
        };
        let refused = {
            let mut w = self.watching.lock().expect("watching poisoned");
            if w.closed || w.live.contains_key(&src) {
                Some(entry)
            } else {
                self.readers
                    .lock()
                    .expect("readers poisoned")
                    .push((Some(src.clone()), rx));
                w.live.insert(src.clone(), entry);
                None
            }
        };
        if let Some(entry) = refused {
            // closed meanwhile (or a duplicate): given back, its reader with it
            self.kernel.unwatch(&entry.target, &entry.reader).await;
            self.kernel.cancel_task(&entry.task).await;
            self.kernel.remove_inbox(&entry.reader);
        }
    }

    /// An `unwatch` frame: the watch the client registered under `src`, given back.
    async fn unwatch(self: Arc<Self>, src: String, next: Next) {
        let entry = self.watching.lock().expect("watching poisoned").live.remove(&src);
        if let Some(entry) = entry {
            drop_watch(&self.kernel, &src, entry, &self.readers).await;
        }
        drop(next);
    }

    /// A `state_subscribe` frame. The state stream is a `watch` on a NODE (`src`):
    /// the leg needs a `watch` line there, and it reads only the events of that
    /// node's subtree it covers. An ingress rule answers "may this client reach
    /// this leg", never "what may it READ". HANDED TO THE LEG
    /// (`inbound_state_subscribe`): the tap is gated as the leg, on the node; its
    /// settle starts the stream, or answers the refusal.
    async fn state_subscribe(self: Arc<Self>, node: AgentId, fid: Value, next: Next) {
        let hand = json!({"type": "inbound_state_subscribe", "node": node.as_str()});
        let (c, n, f) = (Arc::clone(&self), node.clone(), fid.clone());
        let ack = fantastic_io_bridge::inbound::hand_in(&self.kernel, &self.leg, hand, None, move |out| async move {
            c.subscribed(n, &f, &out.result).await;
            drop(next);
        })
        .await;
        if ack.status != fantastic_kernel::Status::Accepted {
            self.send_text(&error_frame(&fid, &ack.result)).await;
        }
    }

    /// A state subscription's settle: the refusal as an `error` frame, or the
    /// snapshot, then the stream.
    async fn subscribed(&self, node: AgentId, fid: &Value, asked: &Value) {
        if fantastic_kernel::refusal::error_text(asked).is_some() {
            self.send_text(&error_frame(fid, asked)).await;
            return;
        }
        let Some(out_tx) = self.out() else { return };
        // First a `state_snapshot` carrying the identity of every agent of the
        // node's subtree the leg covers, so a state-watching view can bootstrap
        // before the first event, THEN the `state_event` stream, cut the same way.
        let snapshot = json!({
            "type": "state_snapshot",
            "agents": self.kernel.state_watch_snapshot(&self.leg, &node),
        });
        let _ = out_tx.send(Out::Text(encode_frame(&snapshot))).await;
        let out_for_cb = out_tx.clone();
        let kernel_for_cb = Arc::clone(&self.kernel);
        let leg_for_cb = self.leg.clone();
        // Conversation rows are not on this stream — an AI agent
        // announces its own rows to its watchers (`watch` on that agent,
        // gated there).
        let token = self.kernel.add_state_subscriber(Arc::new(
            move |event: &Value, body: Option<&fantastic_kernel::Blob>| {
                // THE PER-WATCHER CUT: only the events of the node's
                // subtree the leg covers, a sender it cannot see as
                // `null`.
                let Some(Value::Object(ev)) =
                    kernel_for_cb.state_watch_event(&leg_for_cb, &node, event)
                else {
                    return;
                };
                // `type` FIRST, then the event's own keys — python's
                // `{"type": "state_event", **event}`, which is the byte
                // order a frame diff reads.
                let mut frame = serde_json::Map::new();
                frame.insert("type".to_string(), Value::String("state_event".into()));
                for (k, v) in ev {
                    frame.insert(k, v);
                }
                // Lazy `name` fill — python derives it from the agent
                // record when the event lacks one, so the browser always
                // renders one shape. An agent already gone falls back to
                // its id rather than dropping the key. Python's
                // `rec.get("display_name") or agent_id`: the RAW value,
                // by truthiness.
                if !frame.contains_key("name") {
                    if let Some(aid) = frame.get("agent_id").and_then(Value::as_str) {
                        let name = kernel_for_cb.resolve_target(aid)
                            .and_then(|a| {
                                a.record().meta
                                    .get("display_name")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .map(Value::from)
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
        let refused = {
            let mut s = self.state_subs.lock().expect("state_subs poisoned");
            if s.closed {
                Some(token)
            } else {
                s.tokens.push(token);
                None
            }
        };
        if let Some(t) = refused {
            // the socket closed meanwhile: nothing reads this stream
            self.kernel.remove_state_subscriber(t);
        }
    }

    /// A `state_unsubscribe` frame: every state subscription of this connection goes.
    async fn state_unsubscribe(self: Arc<Self>, next: Next) {
        let tokens = std::mem::take(&mut self.state_subs.lock().expect("state_subs poisoned").tokens);
        for t in tokens {
            self.kernel.remove_state_subscriber(t);
        }
        drop(next);
    }
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
