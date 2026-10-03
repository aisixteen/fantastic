//! The PARENT half of the child's control socket.
//!
//! The child speaks [`crate::CONNECTOR_SRC`]; this serves it. Port of python's
//! `_serve_ctrl`, op for op, so a program authored against either kernel behaves the
//! same.
//!
//! Wire: `[4-byte BE length | kind | rest]`, both directions. Kind `T`: a JSON
//! envelope. Kind `B`: `fantastic_io_bridge::codec`'s binary frame
//! (`[len | JSON header | raw body]`, the header's `_binary_path` naming where the
//! body belongs as a list of steps — a key as a string, taken literally, a list
//! index as an int) — how a `read_stream` chunk or a `write_stream` payload crosses
//! the fd raw, never base64. A raw fd has no WS text/binary frame kind, hence the byte.
//! A kind-B `send` carries its request body beside the payload (`Item::with_binary`); a
//! kind-T one carries none. EVERY reply that holds bytes goes back kind B, whatever
//! the send's kind — python's relay `encode_frame`s each reply dict, with no list of
//! verbs, so a kind-T `read_stream` gets its chunk raw at `["data", "bytes"]`.
//!
//! | child → parent | parent → child |
//! |---|---|
//! | `send{rid,target,payload}` | `reply{rid,data}` |
//! | `emit{target?,payload}` | — |
//! | `watch{src}` / `unwatch{src}` | `event{src,payload}` |
//! | `onmessage` / `offmessage` | `inbox{payload}` |

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use fantastic_bundle::py_truthy;
use fantastic_io_bridge::codec::{decode_binary_frame, encode_binary_frame};
use fantastic_kernel::{AgentId, Blob, Item, Kernel, TaskId};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// ONE frame bound on this wire, both ends, every runtime. A corrupt length must
/// not become a 4 GiB allocation, and a program that means to move 20 MB means to
/// STREAM it. Over-limit is a WIRE VIOLATION — the framing itself is unusable, so
/// there is nothing to reply to and the socket closes; the connector refuses to
/// write such a frame in the first place, naming `write_stream`.
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Exactly ONE job per owner receives the owner's inbox. A re-started durable tool
/// whose predecessor lingers would otherwise handle every inbound `send` twice.
struct OwnerInbox {
    job_id: TaskId,
    proxy: AgentId,
    drain: TaskId,
}

static OWNER_INBOX: OnceLock<Mutex<HashMap<AgentId, OwnerInbox>>> = OnceLock::new();

fn owner_inbox() -> &'static Mutex<HashMap<AgentId, OwnerInbox>> {
    OWNER_INBOX.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A `watch` subscription held on the job's behalf.
struct Sub {
    proxy: AgentId,
    /// The drain's row in the kernel TASK TABLE. It is registered work for as long
    /// as the subscription lives — `ps` lists it and `cancel` reaches it — so it is
    /// released through [`drop_proxy`], never by dropping a detached handle.
    drain: TaskId,
}

/// Write one frame: `[4-byte BE len | kind | rest]`.
async fn write_frame(wr: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>, kind: u8, rest: &[u8]) {
    let mut buf = Vec::with_capacity(5 + rest.len());
    buf.extend_from_slice(&((rest.len() + 1) as u32).to_be_bytes());
    buf.push(kind);
    buf.extend_from_slice(rest);
    let mut g = wr.lock().await;
    let _ = g.write_all(&buf).await;
    let _ = g.flush().await;
}

/// Frame one JSON value onto the socket (kind `T`).
async fn push(wr: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>, v: &Value) {
    // python's `encode_frame` text half — `json.dumps(v, default=str)`, so the
    // separators and the `\uXXXX` escapes match byte for byte (A13.14).
    let body = fantastic_io_bridge::codec::encode_frame(v).into_bytes();
    write_frame(wr, b'T', &body).await;
}

/// Frame an envelope + raw body onto the socket (kind `B`): `body.path` is the
/// slot's steps from the envelope root, written as `_binary_path`, and the head
/// holds `null` there in python's position (in place when the slot exists,
/// appended when not — python's `encode_frame` nulls the bytes where they sat).
async fn push_binary(
    wr: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    mut envelope: Value,
    body: &Blob,
) {
    let _ = fantastic_bundle::path::place(&mut envelope, &body.path, Value::Null);
    write_frame(
        wr,
        b'B',
        &encode_binary_frame(&envelope, &body.path, &body.bytes),
    )
    .await;
}

/// Decode one child frame → `(envelope, raw body at its steps from the envelope
/// root)`. Kind `T`: JSON, no body. Kind `B`: the io_bridge binary frame —
/// python's `decode_frame`: `_binary_path` popped and the slot checked (the
/// envelope keeps `null` there), the body beside it, since a `Value` cannot hold
/// bytes; a frame that names no slot places no body. `None` for a malformed frame
/// (a path that names no slot included — python's decode raises and the frame is
/// skipped), a non-object envelope or an unknown kind.
fn unframe(body: &[u8]) -> Option<(Value, Option<Blob>)> {
    match body.first()? {
        // STRICT (D13): python `_unframe` → `strict_loads`.
        b'T' => fantastic_io_bridge::codec::loads_slice(&body[1..])
            .ok()
            .map(|v| (v, None)),
        b'B' => {
            let f = decode_binary_frame(&body[1..]).ok()?;
            if !f.head.is_object() {
                return None;
            }
            Some((f.head, f.path.map(|p| Blob::new(p, f.body))))
        }
        _ => None,
    }
}

/// The body a frame carries INSIDE its `payload`, at its steps there — the
/// payload python's `m.get("payload")` hands the kernel holds it; anywhere else
/// it is not part of the payload.
fn payload_body(blob: Option<Blob>) -> Option<Blob> {
    blob?.under("payload")
}

/// Frame one queued inbox item into `envelope` (whose `payload` already holds the
/// item's head) — kind `T` for plain JSON, kind `B` when the payload carried raw
/// bytes, `_binary_path` naming where they belong under `payload`. THE ONE CODEC:
/// a `write_stream` mirrored to a watching program reaches it whole, not as a
/// header with a hole where the body was.
async fn push_item(
    wr: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    envelope: Value,
    item: &fantastic_kernel::inbox::InboxItem,
) {
    match &item.body {
        None => push(wr, &envelope).await,
        Some(b) => {
            let at = Blob::new(b.nested_in("payload"), b.bytes.clone());
            push_binary(wr, envelope, &at).await;
        }
    }
}

/// Tear a proxy down: unwatch, DROP THE INBOX ROW, cancel the drain — python's
/// `release`. The `inboxes` removal is the step that used to be missing: a leaked row
/// means `emit`'s auto-vivify path keeps `try_send`ing into a channel nobody drains,
/// forever.
async fn drop_proxy(kernel: &Arc<Kernel>, src: &AgentId, sub: Sub) {
    kernel.unwatch(src, &sub.proxy).await;
    kernel.remove_inbox(&sub.proxy);
    kernel.cancel_task(&sub.drain).await;
}

/// The verbs that DRIVE one job — python `DRIVE_VERBS`. A running program's
/// owner-inbox relay drops one of these whose `job_id` names ANOTHER job (audit
/// BV-9): the fanout mirrors every payload sent to the leg, so a program started
/// by one caller saw another caller's keystrokes, paste bytes and drive calls
/// aimed at that caller's job. One with no `job_id` (bulk) or naming this job is
/// forwarded.
const DRIVE_VERBS: [&str; 6] = [
    "write",
    "resize",
    "paste_image",
    "stop",
    "interrupt",
    "clear",
];

/// The job's own telemetry echoes back through the owner's watcher fanout; drop it so a
/// tool does not receive its own `progress`/`job_started`/`job_done` as inbox mail —
/// drop a drive verb addressed to ANOTHER job ([`DRIVE_VERBS`], audit BV-9) — and
/// drop whatever the leg's gate refuses (python `_inbox_framer`, audit BV-8). The
/// substrate's fanout mirrors every payload to the owner's watchers BEFORE any
/// handler runs, so a sealed leg whose handler refused the call still had it
/// delivered here.
fn inbox_framer(p: &Value, owner: &AgentId, job_id: TaskId, kernel: &Kernel) -> Option<Value> {
    let verb = match p.get("type").and_then(Value::as_str) {
        Some("progress") | Some("output") | Some("job_started") | Some("job_done") => return None,
        v => v.unwrap_or(""),
    };
    // Dropped only when an int `job_id` names ANOTHER job — no id (bulk) or this
    // job's id is forwarded; a bool, a string or a float names no job.
    if DRIVE_VERBS.contains(&verb)
        && p.get("job_id")
            .and_then(Value::as_i64)
            .is_some_and(|j| j != job_id.get())
    {
        return None;
    }
    if crate::gate(&crate::snapshot_meta(owner, kernel), owner, verb).is_some() {
        return None;
    }
    Some(json!({"op": "inbox", "payload": p}))
}

/// Read one `[4-byte BE len | payload]` frame. `None` = the socket is finished:
/// EOF, an error, or a length over [`MAX_FRAME`] — a wire violation the stream
/// cannot be resynchronised after.
async fn read_frame(rd: &mut OwnedReadHalf) -> Option<Vec<u8>> {
    let mut head = [0u8; 4];
    rd.read_exact(&mut head).await.ok()?;
    let n = u32::from_be_bytes(head);
    if n > MAX_FRAME {
        return None;
    }
    let mut body = vec![0u8; n as usize];
    rd.read_exact(&mut body).await.ok()?;
    Some(body)
}

/// Serve one job's control socket until the child closes it — or until `stop` is
/// raised by [`crate::jobs::teardown`].
///
/// Both endings leave by the SAME door: the release block after the loop. That is
/// the whole point of the signal — the previous teardown aborted this task, which
/// skipped that block and leaked every proxy the job held.
pub async fn serve(
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    owner: AgentId,
    job_id: TaskId,
    kernel: Arc<Kernel>,
    stop: Arc<tokio::sync::Notify>,
) {
    let wr = Arc::new(tokio::sync::Mutex::new(wr));
    let mut rd = rd;
    let mut subs: HashMap<AgentId, Sub> = HashMap::new();
    // Only the proxy id — the drain handle lives in OWNER_INBOX (see `onmessage`).
    let mut self_sub: Option<AgentId> = None;

    loop {
        let frame = tokio::select! {
            // A half-read frame is discarded with the connection, so the
            // cancel-safety of `read_exact` does not matter here.
            _ = stop.notified() => None,
            f = read_frame(&mut rd) => f,
        };
        let Some(body) = frame else {
            break; // EOF, a wire violation, or teardown — release and go.
        };
        let Some((m, framed)) = unframe(&body) else {
            continue; // malformed body → skip the frame, never the connection
        };

        match m.get("op").and_then(Value::as_str) {
            Some("send") => {
                let target = m.get("target").and_then(Value::as_str).unwrap_or("");
                // py `m.get("payload") or {}` — TRUTHINESS, so an explicit null, `0`
                // or `[]` is an empty payload rather than a value the kernel has to
                // make sense of.
                let payload = match m.get("payload").filter(|v| py_truthy(v)) {
                    Some(p) => p.clone(),
                    None => json!({}),
                };
                // RELAYED UNTOUCHED, as python's `_serve_ctrl` relays it: one
                // `kernel.send([{target, payload, secret?}])`, no tag added, nothing
                // refused here — the kernel's gate is the only judge.
                // A kind-B frame (bytes alongside, `_binary_path` = `["payload", …]`)
                // carries a request body: the SAME batch item on the binary
                // channel, so it gets its pid, rows, `ps` row and deadline like any
                // other call. Rust's `Value` cannot carry bytes, so the body rides
                // beside the payload, at its path there.
                let mut item = Item::new(AgentId::from(target), payload);
                if let Some(marks) = m.get("secret") {
                    // The program's SECRET marks ride onto the item as sent; the
                    // kernel validates them (a bad mark is a refusal in the reply)
                    // and masks every record of the call.
                    item = item.with_secret(marks.clone());
                }
                if let Some(b) = payload_body(framed) {
                    item = item.with_binary(b.path, b.bytes);
                }
                let out = kernel.send(vec![item]).await.remove(0);
                let (data, body) = (out.result, out.body);
                let env = json!({"op": "reply", "rid": m.get("rid"), "data": data});
                match body {
                    // The verb returned NO chunk — a plain text frame. `Some(b)`
                    // with `b` EMPTY is still a chunk (an empty file), so the
                    // test is the carrier, never the length (C37).
                    None => push(&wr, &env).await,
                    Some(body) => {
                        // A chunk rides raw at `["data", …its slot]` — `["data",
                        // "bytes"]` for a `read_stream` (python's reply dict
                        // holds it there natively).
                        let at = Blob::new(body.nested_in("data"), body.bytes);
                        push_binary(&wr, env, &at).await;
                    }
                }
            }
            Some("emit") => {
                // A program emits on its OWNER agent — the one it runs as.
                let payload = match m.get("payload").filter(|v| py_truthy(v)) {
                    Some(p) => p.clone(),
                    None => json!({}),
                };
                // a payload holding bytes is emitted WHOLE: watchers get the body
                // at its path, as python's `kernel.emit` hands them the dict
                let item = match payload_body(framed) {
                    Some(b) => fantastic_kernel::inbox::InboxItem::binary(payload, b),
                    None => fantastic_kernel::inbox::InboxItem::text(payload),
                };
                kernel.emit_item(&owner, item).await;
            }
            Some("watch") => {
                let Some(src) = m.get("src").and_then(Value::as_str).map(AgentId::from) else {
                    continue;
                };
                if subs.contains_key(&src) {
                    continue; // idempotent per src
                }
                let proxy = AgentId::from(format!("_xbw_{}_{src}", job_id.get()).as_str());
                // THE READER REGISTERS FIRST: `watch` refuses a watcher with no
                // inbox, and the drain task below is what reads this one.
                let mut rx = kernel.ensure_inbox(&proxy);
                // THE KERNEL DECIDES. `watch` is gated exactly like a send (it IS a
                // read of the source) and can refuse. The refusal used to be
                // discarded and the sub registered anyway, so the program held a
                // subscription that simply never fired — deaf, with nothing to
                // diagnose. Tell it, and register NOTHING.
                if let Some(denial) = kernel.watch(&src, proxy.clone()).await {
                    kernel.remove_inbox(&proxy);
                    push(
                        &wr,
                        &json!({"op": "event", "src": src.as_str(), "payload": denial}),
                    )
                    .await;
                    continue;
                }
                let w = Arc::clone(&wr);
                let s = src.clone();
                // REGISTERED as a kernel task `watch_drain` (A4.13): a drain lives
                // as long as the subscription, so it is real work — visible to `ps`
                // and reachable by `cancel` — not a detached `tokio::spawn`.
                let drain = kernel.spawn_task(&owner, "watch_drain", async move {
                    while let Some(p) = rx.recv().await {
                        let env = json!({"op": "event", "src": s.as_str(), "payload": p.payload});
                        push_item(&w, env, &p).await;
                    }
                });
                subs.insert(src, Sub { proxy, drain });
            }
            Some("unwatch") => {
                if let Some(src) = m.get("src").and_then(Value::as_str).map(AgentId::from) {
                    if let Some(sub) = subs.remove(&src) {
                        drop_proxy(&kernel, &src, sub).await;
                    }
                }
            }
            Some("onmessage") => {
                if self_sub.is_some() {
                    continue; // a second onmessage from the SAME job is a no-op
                }
                // Supersede any predecessor OUTSIDE the lock — `unwatch` is async and no
                // std guard may cross an await. Removing it inside the section is what
                // makes the gap safe: it can receive no more mail in the interim.
                let prev = owner_inbox().lock().expect("OWNER_INBOX").remove(&owner);
                if let Some(prev) = prev {
                    kernel.unwatch(&owner, &prev.proxy).await;
                    kernel.remove_inbox(&prev.proxy);
                    kernel.cancel_task(&prev.drain).await;
                }
                let proxy = AgentId::from(format!("_xbself_{}", job_id.get()).as_str());
                // THE READER REGISTERS FIRST — see `watch` above.
                let mut rx = kernel.ensure_inbox(&proxy);
                if let Some(denial) = kernel.watch(&owner, proxy.clone()).await {
                    kernel.remove_inbox(&proxy);
                    push(
                        &wr,
                        &json!({"op": "event", "src": owner.as_str(), "payload": denial}),
                    )
                    .await;
                    continue;
                }
                let w = Arc::clone(&wr);
                // The framer reads the LIVE record per payload; a weak handle, so
                // the drain (a row in the kernel's own task table) never keeps the
                // kernel alive.
                let weak = Arc::downgrade(&kernel);
                let o = owner.clone();
                // REGISTERED as a kernel task `inbox_drain` (A4.13) — see the
                // `watch_drain` above.
                let drain = kernel.spawn_task(&owner, "inbox_drain", async move {
                    while let Some(p) = rx.recv().await {
                        let Some(k) = weak.upgrade() else { break };
                        if let Some(f) = inbox_framer(&p.payload, &o, job_id, &k) {
                            push_item(&w, f, &p).await;
                        }
                    }
                });
                // The REAL handle lives in OWNER_INBOX, because a supersede has to be
                // able to abort a predecessor owned by a DIFFERENT relay task. The
                // local marker keeps only the proxy id.
                owner_inbox().lock().expect("OWNER_INBOX").insert(
                    owner.clone(),
                    OwnerInbox {
                        job_id,
                        proxy: proxy.clone(),
                        drain,
                    },
                );
                self_sub = Some(proxy);
            }
            Some("offmessage") => {
                self_sub = None;
                release_owner_inbox(&kernel, &owner, job_id).await;
            }
            _ => {}
        }
    }

    // The child closed the socket (it exited). Release everything it held.
    for (src, sub) in subs.drain() {
        drop_proxy(&kernel, &src, sub).await;
    }
    if self_sub.take().is_some() {
        release_owner_inbox(&kernel, &owner, job_id).await;
    }
}

/// Give up the owner's inbox — but ONLY if it still names this job. A predecessor
/// tearing down after being superseded must not unhook its replacement.
async fn release_owner_inbox(kernel: &Arc<Kernel>, owner: &AgentId, job_id: TaskId) {
    let mine = {
        let mut g = owner_inbox().lock().expect("OWNER_INBOX");
        match g.get(owner) {
            Some(c) if c.job_id == job_id => g.remove(owner),
            _ => None,
        }
    };
    if let Some(c) = mine {
        kernel.unwatch(owner, &c.proxy).await;
        kernel.remove_inbox(&c.proxy);
        kernel.cancel_task(&c.drain).await;
    }
}
