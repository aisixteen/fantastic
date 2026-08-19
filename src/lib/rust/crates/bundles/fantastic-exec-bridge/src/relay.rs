//! The PARENT half of the child's control socket.
//!
//! The child speaks [`crate::CONNECTOR_SRC`]; this serves it. Port of python's
//! `_serve_ctrl`, op for op, so a program authored against either kernel behaves the
//! same.
//!
//! Wire: `[4-byte BE length | JSON body]`, both directions. That is deliberately NOT
//! `fantastic_io_bridge::codec`, which is a THREE-part binary frame
//! (`[len | JSON header | raw body]`) for carrying bytes without base64. The two share
//! only the length prefix; wiring them together would be a silent interop bug.
//!
//! | child → parent | parent → child |
//! |---|---|
//! | `send{rid,target,payload}` | `reply{rid,data}` |
//! | `emit{target?,payload}` | — |
//! | `watch{src}` / `unwatch{src}` | `event{src,payload}` |
//! | `onmessage` / `offmessage` | `inbox{payload}` |

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use fantastic_kernel::call_guard::guard_call_stack;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A corrupt length must not become a 4 GiB allocation. The wire never legitimately
/// exceeds this; python has no clamp only because its reader does not preallocate.
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Exactly ONE job per owner receives the owner's inbox. A re-started durable tool
/// whose predecessor lingers would otherwise handle every inbound `send` twice.
struct OwnerInbox {
    job_id: String,
    proxy: AgentId,
    drain: JoinHandle<()>,
}

static OWNER_INBOX: OnceLock<Mutex<HashMap<AgentId, OwnerInbox>>> = OnceLock::new();

fn owner_inbox() -> &'static Mutex<HashMap<AgentId, OwnerInbox>> {
    OWNER_INBOX.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A `watch` subscription held on the job's behalf.
struct Sub {
    proxy: AgentId,
    drain: JoinHandle<()>,
}

/// Frame one JSON value onto the socket.
async fn push(wr: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>, v: &Value) {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    let mut g = wr.lock().await;
    let _ = g.write_all(&buf).await;
    let _ = g.flush().await;
}

/// Make a proxy inbox, then watch through it.
///
/// Rust's `kernel.inboxes` holds only the Sender, and `Kernel::watch` auto-vivifies with
/// a Receiver it immediately drops — so the receiver must be created HERE, before the
/// watch. `watch` guards on `contains_key`, which is what makes pre-inserting safe.
fn make_proxy(kernel: &Arc<Kernel>, proxy: &AgentId) -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel(kernel.inbox_bound);
    kernel.inboxes.insert(proxy.clone(), tx);
    rx
}

/// Tear a proxy down. The `inboxes` removal is the rust-only step: a leaked row means
/// `emit`'s auto-vivify path keeps `try_send`ing into a channel nobody drains, forever.
async fn drop_proxy(kernel: &Arc<Kernel>, src: &AgentId, sub: Sub) {
    kernel.unwatch(src, &sub.proxy).await;
    kernel.inboxes.remove(&sub.proxy);
    sub.drain.abort();
}

/// The job's own telemetry echoes back through the owner's watcher fanout; drop it so a
/// tool does not receive its own `progress`/`job_started`/`job_done` as inbox mail.
fn inbox_framer(p: &Value) -> Option<Value> {
    match p.get("type").and_then(Value::as_str) {
        Some("progress") | Some("job_started") | Some("job_done") => None,
        _ => Some(json!({"op": "inbox", "payload": p})),
    }
}

/// Serve one job's control socket until the child closes it.
pub async fn serve(
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    owner: AgentId,
    job_id: String,
    kernel: Arc<Kernel>,
) {
    let wr = Arc::new(tokio::sync::Mutex::new(wr));
    let mut rd = rd;
    let mut subs: HashMap<AgentId, Sub> = HashMap::new();
    // Only the proxy id — the drain handle lives in OWNER_INBOX (see `onmessage`).
    let mut self_sub: Option<AgentId> = None;

    loop {
        let mut head = [0u8; 4];
        if rd.read_exact(&mut head).await.is_err() {
            break; // EOF — the child is gone.
        }
        let n = u32::from_be_bytes(head);
        if n > MAX_FRAME {
            break;
        }
        let mut body = vec![0u8; n as usize];
        if rd.read_exact(&mut body).await.is_err() {
            break;
        }
        let Ok(m) = serde_json::from_slice::<Value>(&body) else {
            continue; // malformed body → skip the frame, never the connection
        };

        match m.get("op").and_then(Value::as_str) {
            Some("send") => {
                let target = m.get("target").and_then(Value::as_str).unwrap_or("");
                let mut payload = m.get("payload").cloned().unwrap_or_else(|| json!({}));
                // Python gets recursion guarding free inside `Agent.send`; rust's
                // `Kernel::send` does not, so the guard is explicit here. Behavioural
                // parity, not code parity. On refusal the reply still goes out, or the
                // child's blocking `q.get()` would hang forever.
                let data = match guard_call_stack(owner.as_str(), &mut payload) {
                    Err(e) => json!({ "error": e }),
                    Ok(()) => kernel.send(&AgentId::from(target), payload).await,
                };
                push(
                    &wr,
                    &json!({"op": "reply", "rid": m.get("rid"), "data": data}),
                )
                .await;
            }
            Some("emit") => {
                let tgt = m
                    .get("target")
                    .and_then(Value::as_str)
                    .map(AgentId::from)
                    .unwrap_or_else(|| owner.clone());
                let payload = m.get("payload").cloned().unwrap_or_else(|| json!({}));
                kernel.emit(&tgt, payload).await;
            }
            Some("watch") => {
                let Some(src) = m.get("src").and_then(Value::as_str).map(AgentId::from) else {
                    continue;
                };
                if subs.contains_key(&src) {
                    continue; // idempotent per src
                }
                let proxy = AgentId::from(format!("_xbw_{job_id}_{src}").as_str());
                let mut rx = make_proxy(&kernel, &proxy);
                kernel.watch(&src, proxy.clone()).await;
                let w = Arc::clone(&wr);
                let s = src.clone();
                let drain = tokio::spawn(async move {
                    while let Some(p) = rx.recv().await {
                        push(&w, &json!({"op": "event", "src": s.as_str(), "payload": p})).await;
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
                    kernel.inboxes.remove(&prev.proxy);
                    prev.drain.abort();
                }
                let proxy = AgentId::from(format!("_xbself_{job_id}").as_str());
                let mut rx = make_proxy(&kernel, &proxy);
                kernel.watch(&owner, proxy.clone()).await;
                let w = Arc::clone(&wr);
                let drain = tokio::spawn(async move {
                    while let Some(p) = rx.recv().await {
                        if let Some(f) = inbox_framer(&p) {
                            push(&w, &f).await;
                        }
                    }
                });
                // The REAL handle lives in OWNER_INBOX, because a supersede has to be
                // able to abort a predecessor owned by a DIFFERENT relay task. The
                // local marker keeps only the proxy id.
                owner_inbox().lock().expect("OWNER_INBOX").insert(
                    owner.clone(),
                    OwnerInbox {
                        job_id: job_id.clone(),
                        proxy: proxy.clone(),
                        drain,
                    },
                );
                self_sub = Some(proxy);
            }
            Some("offmessage") => {
                self_sub = None;
                release_owner_inbox(&kernel, &owner, &job_id).await;
            }
            _ => {}
        }
    }

    // The child closed the socket (it exited). Release everything it held.
    for (src, sub) in subs.drain() {
        drop_proxy(&kernel, &src, sub).await;
    }
    if self_sub.take().is_some() {
        release_owner_inbox(&kernel, &owner, &job_id).await;
    }
}

/// Give up the owner's inbox — but ONLY if it still names this job. A predecessor
/// tearing down after being superseded must not unhook its replacement.
async fn release_owner_inbox(kernel: &Arc<Kernel>, owner: &AgentId, job_id: &str) {
    let mine = {
        let mut g = owner_inbox().lock().expect("OWNER_INBOX");
        match g.get(owner) {
            Some(c) if c.job_id == job_id => g.remove(owner),
            _ => None,
        }
    };
    if let Some(c) = mine {
        kernel.unwatch(owner, &c.proxy).await;
        kernel.inboxes.remove(&c.proxy);
        c.drain.abort();
    }
}
