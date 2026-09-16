//! The WebSocket frame loop — the rust port of python's
//! `PB/io/web_ws/src/web_ws/_proxy.py::run`.
//!
//! ```text
//! C→S {type:"call",  target, payload, id}     → kernel.send → reply
//! C→S {type:"emit",  target, payload}         → kernel.emit (no reply)
//! C→S {type:"watch"|"unwatch", src}           → (un)register a watcher
//! C→S {type:"state_subscribe"|"state_unsubscribe"}
//! S→C {type:"reply", id, data} | {type:"error", id, …}
//! S→C {type:"event", payload}                 → drained from the watcher inbox
//! ```
//!
//! Every server→client TEXT frame leaves through
//! [`fantastic_io_bridge::codec::encode_frame`] — python's
//! `json.dumps(envelope, default=str)`, `, `/`: ` separators and
//! `ensure_ascii` — so a `·` or a `↑` in a reply is `·` / `↑` on the
//! wire exactly as python sends it (A13.14).

use crate::AppState;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use fantastic_io_bridge::codec::encode_frame;
use fantastic_kernel::{AgentId, Item, Kernel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

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

/// Maximum size of a single binary WS frame's blob payload.
#[cfg(not(test))]
pub(crate) const MAX_CHUNK_SIZE: usize = 1_048_576; // 1 MB

/// Maximum total size of a reassembled chunked upload.
#[cfg(not(test))]
pub(crate) const MAX_UPLOAD_SIZE: usize = 100 * 1_048_576; // 100 MB

// UNDER TEST THE CAPS ARE TINY. The cap check counts REAL bytes, so proving the
// total cap fires meant actually pushing 101 MB across a websocket — 83 s of
// the rust suite's 123 for ONE test. What is under test is the ENFORCEMENT,
// never the constant's value, which no assertion reads.
#[cfg(test)]
pub(crate) const MAX_CHUNK_SIZE: usize = 4 * 1024; // 4 KB
#[cfg(test)]
pub(crate) const MAX_UPLOAD_SIZE: usize = 16 * 1024; // 16 KB

/// Maximum concurrent in-flight chunked uploads per WS connection.
const MAX_CONCURRENT_UPLOADS: usize = 16;

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

/// WebSocket proxy loop.
///
/// `host_agent_id` is the URL the browser connected to
/// (`ws://host/<host_id>/ws`). It is auto-watched on connect so events emitted
/// on its inbox reach the page without an explicit `watch` frame, and it is the
/// identity browser-driven calls run as.
pub(crate) async fn ws_loop(state: AppState, socket: WebSocket, host_agent_id: AgentId) {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_CLIENT_HEX: AtomicU64 = AtomicU64::new(0);
    let n = NEXT_CLIENT_HEX.fetch_add(1, Ordering::SeqCst);
    let client_id = AgentId::from(format!("_ws_{n:06x}").as_str());

    // Per-WS chunked-upload reassembly state. Lives in this scope so it drops
    // on disconnect — no GC task needed for the "dead client" case.
    let pending_uploads: Arc<std::sync::Mutex<std::collections::HashMap<String, ChunkBuffer>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    let (mut sink, mut stream) = socket.split();

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Out>(state.kernel.inbox_bound);
    // Hook the synthetic client inbox into the kernel. THE READER REGISTERS
    // FIRST, and the queue drops the OLDEST at the bound — a browser that
    // stalls catches up on the freshest frames rather than freezing on stale.
    let mut inbox_rx = state.kernel.ensure_inbox(&client_id);

    // Auto-watch the host agent (python parity). Track every watch so cleanup
    // can unwatch them all on disconnect.
    let watching: Arc<tokio::sync::Mutex<HashSet<AgentId>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::from_iter([
            host_agent_id.clone()
        ])));
    state.kernel.watch(&host_agent_id, client_id.clone()).await;

    // Pending `call` tasks — cancelled on disconnect so long-lived sends
    // release their locks immediately.
    let pending_calls: Arc<std::sync::Mutex<Vec<fantastic_kernel::TaskId>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // State-stream subscription tokens — removed on disconnect.
    let state_unsubs: Arc<std::sync::Mutex<Vec<fantastic_kernel::SubscriberToken>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let drain_task = tokio::spawn({
        let out_tx = out_tx.clone();
        async move {
            while let Some(item) = inbox_rx.recv().await {
                let frame = json!({"type": "event", "payload": item.payload});
                // A QUEUED PAYLOAD HOLDING BYTES IS A BINARY FRAME — python's
                // `encode_outbound` finds the bytes inside the payload dict and
                // sends `[4B len | head with _binary_path | body]`. The rust
                // carrier keeps the body beside the payload, so the path is the
                // item's own path nested under this envelope's `payload`.
                let out = match &item.body {
                    None => Out::Text(encode_frame(&frame)),
                    Some((path, body)) => {
                        let mut head = frame;
                        head["_binary_path"] = json!(format!("payload.{path}"));
                        Out::Binary(fantastic_io_bridge::codec::encode_binary_frame(&head, body))
                    }
                };
                if out_tx.send(out).await.is_err() {
                    break;
                }
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
                            code: CLOSE_INTERNAL_ERROR,
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
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(bytes)) => {
                handle_binary_frame(&state, &client_id, &out_tx, &pending_uploads, bytes).await;
                continue;
            }
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
        let Ok(env): Result<Value, _> = serde_json::from_str(&text) else {
            continue;
        };
        let ty = env.get("type").and_then(Value::as_str).unwrap_or("");
        let target = env.get("target").and_then(Value::as_str).map(AgentId::from);
        let payload = env.get("payload").cloned().unwrap_or(Value::Null);
        let id = env.get("id").and_then(Value::as_str).map(str::to_string);
        let kernel = Arc::clone(&state.kernel);
        let sender_for_scope = host_agent_id.clone();
        let out = out_tx.clone();
        match ty {
            "call" => {
                let Some(target) = target else {
                    if let Some(id) = id {
                        let frame = json!({"type":"error","id":id,"error":"call requires target"});
                        let _ = out.send(Out::Text(encode_frame(&frame))).await;
                    }
                    continue;
                };
                let frame_id = id.clone();
                // REGISTERED: a frame handler DISPATCHES, so it is real work —
                // a `ps` row, reachable by `cancel`, and its send parented to
                // it instead of becoming a root.
                let spawner = Arc::clone(&state.kernel);
                let handle = spawner.spawn_task(&sender_for_scope, "ws_call", async move {
                    // THE FRAME ID IS THE EXECUTION ID. One id space: the same
                    // id arriving twice — a double-click, a second tab, a
                    // reconnect replaying its queue — is ONE intent, deduped
                    // and joined by the ledger instead of running twice.
                    let reply = kernel
                        .send(vec![Item::from_frame(
                            target.clone(),
                            payload,
                            frame_id.as_deref(),
                        )])
                        .await
                        .remove(0)
                        .result;
                    if let Some(id) = id {
                        // Verb-level errors travel inside `data.error` (python's
                        // wire shape); a `type:"error"` frame is reserved for
                        // out-of-band failures.
                        let frame = json!({"type": "reply", "id": id, "data": reply});
                        let _ = out.send(Out::Text(encode_frame(&frame))).await;
                    }
                });
                track_call(&state.kernel, &pending_calls, handle);
            }
            "emit" => {
                let Some(target) = target else { continue };
                fantastic_kernel::send::with_sender(sender_for_scope, async {
                    kernel.emit(&target, payload).await
                })
                .await;
            }
            "watch" => {
                let Some(src) = env.get("src").and_then(Value::as_str).map(AgentId::from) else {
                    continue;
                };
                let mut w = watching.lock().await;
                if !w.contains(&src) {
                    // UNDER THE LEG'S OWN IDENTITY, so the kernel's gate sees a
                    // real caller. Without it the kernel reads the frame as an
                    // OPERATOR watch and the two-sided gate never runs.
                    let denial = fantastic_kernel::send::with_sender(
                        host_agent_id.clone(),
                        kernel.watch(&src, client_id.clone()),
                    )
                    .await;
                    if let Some(d) = denial {
                        let id = env.get("id").cloned().unwrap_or(Value::Null);
                        let frame = json!({"type": "error", "id": id, "data": d});
                        let _ = out.send(Out::Text(encode_frame(&frame))).await;
                        continue;
                    }
                    w.insert(src);
                }
            }
            "unwatch" => {
                let Some(src) = env.get("src").and_then(Value::as_str).map(AgentId::from) else {
                    continue;
                };
                let mut w = watching.lock().await;
                if w.remove(&src) {
                    kernel.unwatch(&src, &client_id).await;
                }
            }
            "state_subscribe" => {
                // The state stream is a KERNEL-WIDE read: every send's sender,
                // target, verb and payload summary, for every agent in the
                // tree. So it is gated on the ROOT — an ingress rule answers
                // "may this client reach this leg", never "what may it READ".
                let denial = fantastic_kernel::send::with_sender(host_agent_id.clone(), async {
                    kernel.authorize_state_subscribe(&host_agent_id)
                })
                .await;
                if let Some(d) = denial {
                    let id = env.get("id").cloned().unwrap_or(Value::Null);
                    let frame = json!({"type": "error", "id": id, "data": d});
                    let _ = out.send(Out::Text(encode_frame(&frame))).await;
                    continue;
                }
                // python parity: first a `state_snapshot` carrying every
                // agent's identity so a state-watching view can bootstrap
                // before the first event, THEN the `state_event` stream.
                let snapshot = json!({"type": "state_snapshot", "agents": kernel.state_snapshot()});
                let _ = out_tx.send(Out::Text(encode_frame(&snapshot))).await;
                let out_for_cb = out_tx.clone();
                let kernel_for_cb = Arc::clone(&kernel);
                let token = kernel.add_state_subscriber(Arc::new(
                    move |event: &Value, body: Option<&(String, Vec<u8>)>| {
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
                        // its id rather than dropping the key.
                        if !frame.contains_key("name") {
                            if let Some(aid) = frame.get("agent_id").and_then(Value::as_str) {
                                let name = kernel_for_cb
                                    .agents
                                    .get(&AgentId::from(aid))
                                    .and_then(|a| a.display_name())
                                    .unwrap_or_else(|| aid.to_string());
                                frame.insert("name".into(), Value::String(name));
                            }
                        }
                        let mut frame = Value::Object(frame);
                        // Best-effort: drop if the drain channel is gone
                        // (disconnected mid-fanout).
                        let out = match body {
                            None => Out::Text(encode_frame(&frame)),
                            Some((path, bytes)) => {
                                frame["_binary_path"] = json!(format!("payload.{path}"));
                                Out::Binary(fantastic_io_bridge::codec::encode_binary_frame(
                                    &frame, bytes,
                                ))
                            }
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
            _ => {}
        }
    }

    // ─── Cleanup on disconnect (python's `finally` block) ──────────
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
        let srcs: Vec<AgentId> = watching.lock().await.drain().collect();
        for src in srcs {
            state.kernel.unwatch(&src, &client_id).await;
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
    drain_task.abort();
    drop(out_tx);
    let _ = send_task.await;
}

// ─── binary frames ──────────────────────────────────────────────────
//
// A11.19 (the codec-envelope binary frame + deleting the chunked-upload
// protocol) co-lands with C40's ws_loop change; until then the single-frame and
// chunked paths below are unchanged.

/// Per-upload reassembly buffer, keyed by `upload_id` in a per-WS map.
pub(crate) struct ChunkBuffer {
    /// Chunks indexed by `chunk_index`; a BTreeMap concatenates in order
    /// regardless of arrival order.
    chunks: std::collections::BTreeMap<u32, Vec<u8>>,
    /// Declared total chunk count (must match across every chunk).
    total: u32,
    /// Cumulative byte count across all chunks received so far.
    cumulative_bytes: usize,
    /// First chunk's header, chunking fields stripped.
    base_header: Value,
}

/// Send a binary-dispatch reply on the correct WS channel. An empty `body` is a
/// plain `{type:reply, id, data}` TEXT frame; a non-empty body is a BINARY
/// frame `[4B len|header|body]` — raw bytes, never base64.
async fn send_binary_reply(
    out_tx: &tokio::sync::mpsc::Sender<Out>,
    id: Option<Value>,
    reply: Value,
    body: Vec<u8>,
) {
    if body.is_empty() {
        let frame = json!({"type": "reply", "id": id, "data": reply});
        let _ = out_tx.send(Out::Text(encode_frame(&frame))).await;
        return;
    }
    let mut header = json!({"type": "reply", "id": id, "data": reply});
    header["_binary_path"] = json!("data.bytes");
    let wire = fantastic_io_bridge::codec::encode_binary_frame(&header, &body);
    let _ = out_tx.send(Out::Binary(wire)).await;
}

/// Decode a binary WS frame `[4-byte BE header_len | JSON header | blob]` and
/// dispatch it through [`Kernel::send_with_binary`].
async fn handle_binary_frame(
    state: &AppState,
    client_id: &AgentId,
    out_tx: &tokio::sync::mpsc::Sender<Out>,
    pending_uploads: &Arc<std::sync::Mutex<std::collections::HashMap<String, ChunkBuffer>>>,
    bytes: Vec<u8>,
) {
    // Every malformed-frame branch below DROPS the frame silently, as python's
    // web_ws proxy does — nothing rides a rust-only log line (A15.3).
    if bytes.len() < 4 {
        return;
    }
    let hdr_len = u32::from_be_bytes(match bytes[0..4].try_into() {
        Ok(arr) => arr,
        Err(_) => return,
    }) as usize;
    if 4usize.saturating_add(hdr_len) > bytes.len() {
        return;
    }
    let header: Value = match serde_json::from_slice(&bytes[4..4 + hdr_len]) {
        Ok(v) => v,
        Err(_) => return,
    };
    let blob = bytes[4 + hdr_len..].to_vec();
    let id = header.get("id").cloned();

    if blob.len() > MAX_CHUNK_SIZE {
        let frame = json!({
            "type": "error",
            "id": id,
            "error": format!(
                "binary frame: blob {} bytes exceeds chunk cap {}",
                blob.len(),
                MAX_CHUNK_SIZE,
            ),
        });
        let _ = out_tx.send(Out::Text(encode_frame(&frame))).await;
        return;
    }

    let upload_id = header
        .get("upload_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(upload_id) = upload_id {
        handle_chunked_frame(
            state,
            client_id,
            out_tx,
            pending_uploads,
            upload_id,
            header,
            id.unwrap_or(Value::Null),
            blob,
        )
        .await;
        return;
    }

    let Some(target_str) = header.get("target").and_then(Value::as_str) else {
        return;
    };
    let target = AgentId::from(target_str);
    let kernel = Arc::clone(&state.kernel);
    let sender_for_scope = client_id.clone();
    let out = out_tx.clone();
    tokio::spawn(async move {
        let (reply, body) = fantastic_kernel::send::with_sender(sender_for_scope, async {
            kernel.send_with_binary(&target, header, blob).await
        })
        .await;
        send_binary_reply(&out, id, reply, body).await;
    });
}

/// Pure (no-I/O) chunk-ingest step. Returns `Ok(None)` to ack-and-wait,
/// `Ok(Some(..))` when the upload is complete, or `Err` on a
/// concurrent-cap / total-mismatch / size-cap violation.
pub(crate) fn ingest_chunk(
    map: &mut std::collections::HashMap<String, ChunkBuffer>,
    upload_id: &str,
    chunk_index: u32,
    total_chunks: u32,
    blob: Vec<u8>,
    base_header: &Value,
) -> Result<Option<(Value, Vec<u8>)>, String> {
    if !map.contains_key(upload_id) && map.len() >= MAX_CONCURRENT_UPLOADS {
        return Err(format!(
            "chunked upload: concurrent upload cap {MAX_CONCURRENT_UPLOADS} reached",
        ));
    }
    let entry_existed = map.contains_key(upload_id);
    let buf = map
        .entry(upload_id.to_string())
        .or_insert_with(|| ChunkBuffer {
            chunks: std::collections::BTreeMap::new(),
            total: total_chunks,
            cumulative_bytes: 0,
            base_header: base_header.clone(),
        });
    if entry_existed && buf.total != total_chunks {
        let prior = buf.total;
        map.remove(upload_id);
        return Err(format!(
            "chunked upload: total_chunks mismatch (saw {prior}, now {total_chunks})",
        ));
    }
    let projected = buf.cumulative_bytes.saturating_add(blob.len());
    if projected > MAX_UPLOAD_SIZE {
        map.remove(upload_id);
        return Err(format!(
            "chunked upload: projected {projected} bytes exceeds total cap {MAX_UPLOAD_SIZE}",
        ));
    }
    buf.cumulative_bytes = projected;
    buf.chunks.insert(chunk_index, blob);
    if buf.chunks.len() as u32 == buf.total {
        let removed = map.remove(upload_id).expect("entry just inserted");
        let mut full = Vec::with_capacity(removed.cumulative_bytes);
        for (_, chunk) in removed.chunks {
            full.extend_from_slice(&chunk);
        }
        Ok(Some((removed.base_header, full)))
    } else {
        Ok(None)
    }
}

/// Handle one chunk of a chunked upload: validate, delegate assembly to
/// [`ingest_chunk`], ack non-final chunks, dispatch once complete.
#[allow(clippy::too_many_arguments)]
async fn handle_chunked_frame(
    state: &AppState,
    client_id: &AgentId,
    out_tx: &tokio::sync::mpsc::Sender<Out>,
    pending_uploads: &Arc<std::sync::Mutex<std::collections::HashMap<String, ChunkBuffer>>>,
    upload_id: String,
    mut header: Value,
    id: Value,
    blob: Vec<u8>,
) {
    let err_frame = |msg: String| json!({"type": "error", "id": id, "error": msg});
    let chunk_index = match header.get("chunk_index").and_then(Value::as_u64) {
        Some(n) if n <= u32::MAX as u64 => n as u32,
        _ => {
            let f = err_frame("chunked upload: chunk_index required (u32)".to_string());
            let _ = out_tx.send(Out::Text(encode_frame(&f))).await;
            return;
        }
    };
    let total_chunks = match header.get("total_chunks").and_then(Value::as_u64) {
        Some(n) if n >= 1 && n <= u32::MAX as u64 => n as u32,
        _ => {
            let f = err_frame("chunked upload: total_chunks required (u32 ≥ 1)".to_string());
            let _ = out_tx.send(Out::Text(encode_frame(&f))).await;
            return;
        }
    };
    if chunk_index >= total_chunks {
        let f = err_frame(format!(
            "chunked upload: chunk_index {chunk_index} >= total_chunks {total_chunks}",
        ));
        let _ = out_tx.send(Out::Text(encode_frame(&f))).await;
        return;
    }
    // The header `final` flag is informational — assembly is triggered by the
    // chunk count against `total_chunks` (the contract), so an early `final` is
    // held until the remaining chunks arrive.
    if let Some(obj) = header.as_object_mut() {
        obj.remove("upload_id");
        obj.remove("chunk_index");
        obj.remove("total_chunks");
        obj.remove("final");
    }
    let outcome: Result<Option<(Value, Vec<u8>)>, String> = {
        let mut map = pending_uploads.lock().expect("pending_uploads poisoned");
        ingest_chunk(
            &mut map,
            &upload_id,
            chunk_index,
            total_chunks,
            blob,
            &header,
        )
    };
    let assembled = match outcome {
        Ok(v) => v,
        Err(msg) => {
            let f = err_frame(msg);
            let _ = out_tx.send(Out::Text(encode_frame(&f))).await;
            return;
        }
    };
    match assembled {
        Some((base_header, full_blob)) => {
            let Some(target_str) = base_header.get("target").and_then(Value::as_str) else {
                let f = err_frame("chunked upload: assembled header missing target".to_string());
                let _ = out_tx.send(Out::Text(encode_frame(&f))).await;
                return;
            };
            let target = AgentId::from(target_str);
            let kernel = Arc::clone(&state.kernel);
            let sender_for_scope = client_id.clone();
            let out = out_tx.clone();
            tokio::spawn(async move {
                let (reply, body) = fantastic_kernel::send::with_sender(sender_for_scope, async {
                    kernel
                        .send_with_binary(&target, base_header, full_blob)
                        .await
                })
                .await;
                send_binary_reply(&out, Some(id), reply, body).await;
            });
        }
        None => {
            // Non-final chunk — ack so the client can flow-control its pipeline.
            let f = json!({
                "type": "chunk_ack",
                "upload_id": upload_id,
                "chunk_index": chunk_index,
            });
            let _ = out_tx.send(Out::Text(encode_frame(&f))).await;
        }
    }
}
