//! THE PEER HALF of the WS leg: calls the host makes INTO the page — python
//! `web_ws._proxy.run`'s `_call_page` / `_on_hello` / `_on_answer` and the
//! disconnect settle in its `finally` (owner, 2026-09-30).
//!
//! A page's kernel says who it is (`hello {guid}`); from then on the host kernel
//! can address its agents as `<guid>|<agent>` (kernel `peers`), and every such
//! call rides THIS socket as an outbound `call` frame, answered by an inbound
//! `reply`/`error` with the same id. This leg only carries the frames — the host
//! kernel routes and gates, the page kernel gates its own agents. One guid per
//! connection, pinned by the first hello.

use super::{error_frame, gate_leg, send_envelope, Out, INBOUND_PAYLOAD_KEY};
use fantastic_io_bridge::authorizer::Decision;
use fantastic_kernel::refusal::{bad, refusal, TRANSPORT_ERROR};
use fantastic_kernel::{AgentId, Kernel, PeerReply, PeerRoute};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::WeakSender;
use tokio::sync::oneshot;

/// This connection's peer: the pinned guid, its route, and the calls in flight.
#[derive(Default)]
pub(super) struct PeerLink {
    /// The guid the first accepted hello pinned, and the route registered for it.
    pinned: Option<(String, Arc<PeerRoute>)>,
    /// The calls waiting for an answer, by frame id (`h<n>`).
    calls: Arc<Mutex<Calls>>,
}

/// The outbound calls of one connection.
#[derive(Default)]
struct Calls {
    /// The last frame id handed out — ids are `h1`, `h2`, … per connection.
    seq: u64,
    /// Frame id → the waiting call.
    waiting: HashMap<String, oneshot::Sender<PeerReply>>,
    /// Set once the socket is gone: a call that reaches the route after that
    /// settles at once instead of waiting on a page that can no longer answer.
    gone: bool,
}

/// The one ending for a call the page will never answer.
fn disconnected(guid: &str) -> Value {
    refusal(
        TRANSPORT_ERROR,
        format!("the page {guid} disconnected before answering"),
    )
}

/// Takes a call's entry back out of the table on every exit — an answer, a
/// disconnect, or the caller giving up (its timeout or `cancel` drops the
/// future mid-wait). Python's `finally: out_calls.pop(fid, None)`.
struct Forget {
    calls: Arc<Mutex<Calls>>,
    fid: String,
}

impl Drop for Forget {
    fn drop(&mut self) {
        self.calls
            .lock()
            .expect("peer calls poisoned")
            .waiting
            .remove(&self.fid);
    }
}

/// ONE call into the page: an outbound `{type:"call", id, target, payload,
/// caller, secret?}` frame, resolved by the inbound answer carrying the same id.
/// `secret` is the item's marks, verbatim, so the PAGE kernel masks its records
/// of this call too; it is on the frame only when the item marked paths. A body
/// rides the binary channel at `payload.<its path>` — where python's payload
/// dict holds it and where the page's decoder puts it back.
// One argument per python `_call_page` input (agent_id, payload, caller,
// secret) plus the connection's own state and the raw body beside the payload.
#[allow(clippy::too_many_arguments)]
async fn call_page(
    calls: Arc<Mutex<Calls>>,
    out: WeakSender<Out>,
    guid: String,
    agent_id: String,
    mut payload: Value,
    caller: String,
    secret: Vec<String>,
    body: Option<fantastic_kernel::Blob>,
) -> PeerReply {
    let (fid, rx) = {
        let mut c = calls.lock().expect("peer calls poisoned");
        if c.gone {
            return (disconnected(&guid), None);
        }
        c.seq += 1;
        let fid = format!("h{}", c.seq);
        let (tx, rx) = oneshot::channel();
        c.waiting.insert(fid.clone(), tx);
        (fid, rx)
    };
    let _forget = Forget {
        calls: Arc::clone(&calls),
        fid: fid.clone(),
    };
    // The route holds the sink WEAKLY: a call still in flight must not keep a
    // closed socket's writer alive.
    let Some(out) = out.upgrade() else {
        return (disconnected(&guid), None);
    };
    // the slot python's dict holds the bytes in: `null` on the head, in place
    // (the kernel admitted the path at launch)
    if let Some(b) = &body {
        let _ = b.place(&mut payload, Value::Null);
    }
    let mut envelope = json!({
        "type": "call",
        "id": fid,
        "target": agent_id,
        "payload": payload,
        "caller": caller,
    });
    // the item's marks, so the PAGE kernel masks its records of this call too
    if !secret.is_empty() {
        envelope["secret"] = json!(secret);
    }
    send_envelope(&out, envelope, body, INBOUND_PAYLOAD_KEY).await;
    drop(out);
    // The sender side is dropped only by the disconnect settle, which answers
    // first — a closed channel still means the page is gone.
    rx.await.unwrap_or_else(|_| (disconnected(&guid), None))
}

impl PeerLink {
    /// `hello {guid}` — make this connection the page kernel's address. Python
    /// `_on_hello`: the guid is checked first (a malformed one is answered even on
    /// a sealed leg), then the leg's gate (a sealed leg carries no peer, and says
    /// nothing — the calls it refuses teach), then the pin, then the kernel's own
    /// one-route-per-guid rule.
    pub(super) async fn hello(
        &mut self,
        kernel: &Arc<Kernel>,
        leg: &AgentId,
        out: &tokio::sync::mpsc::Sender<Out>,
        frame: &Value,
        fid: &Value,
    ) {
        let guid = frame
            .get("guid")
            .and_then(Value::as_str)
            .filter(|g| !g.is_empty() && !g.contains('|'));
        let Some(guid) = guid else {
            let r = bad(
                "hello",
                "guid",
                "the page kernel's guid, a non-empty id without `|`",
            );
            answer_error(out, fid, &r).await;
            return;
        };
        if !matches!(gate_leg(kernel, leg, "hello", frame), Decision::Allow) {
            return;
        }
        if let Some((pinned, _)) = &self.pinned {
            if pinned != guid {
                let r = bad(
                    "hello",
                    "guid",
                    &format!("{pinned} (pinned by this connection's first hello)"),
                );
                answer_error(out, fid, &r).await;
            }
            return;
        }
        let route = {
            let calls = Arc::clone(&self.calls);
            let weak = out.downgrade();
            let g = guid.to_string();
            PeerRoute::new(
                leg.0.clone(),
                move |agent_id, payload, caller, secret, body| {
                    Box::pin(call_page(
                        Arc::clone(&calls),
                        weak.clone(),
                        g.clone(),
                        agent_id,
                        payload,
                        caller,
                        secret,
                        body,
                    ))
                },
            )
        };
        if let Some(taken) = kernel.register_peer(guid, &route) {
            answer_error(out, fid, &taken).await;
            return;
        }
        self.pinned = Some((guid.to_string(), route));
    }

    /// An inbound `reply`/`error` answers one of OUR outbound calls — its `data`
    /// is that call's outcome (a page-side refusal included), a non-object
    /// wrapped as `{result}`. An id we are not waiting on is ignored. Python
    /// `_on_answer`.
    ///
    /// `body` is the frame's raw body at its envelope path (`["data", …]`): the
    /// reply holds it at the rest of that path — under `result` when `data` was
    /// not an object and got wrapped, as python's dict holds it.
    pub(super) fn answer(&self, frame: &Value, body: Option<fantastic_kernel::Blob>) {
        let Some(fid) = frame.get("id").and_then(Value::as_str) else {
            return;
        };
        let waiting = self
            .calls
            .lock()
            .expect("peer calls poisoned")
            .waiting
            .remove(fid);
        let Some(tx) = waiting else { return };
        let (data, wrapped) = match frame.get("data") {
            Some(d @ Value::Object(_)) => (d.clone(), false),
            other => (
                json!({"result": other.cloned().unwrap_or(Value::Null)}),
                true,
            ),
        };
        let body = body.and_then(|b| {
            let rest = b.path.get(1..).unwrap_or_default().to_vec();
            let path = if wrapped {
                let mut p = vec![fantastic_kernel::Step::from("result")];
                p.extend(rest);
                p
            } else {
                rest
            };
            (!path.is_empty()).then(|| fantastic_kernel::Blob::new(path, b.bytes))
        });
        let _ = tx.send((data, body));
    }

    /// The page is gone: it is no longer an address, and every call still
    /// waiting on it settles NOW with the reason — never a hang, never a retry.
    pub(super) fn close(&mut self, kernel: &Kernel) {
        let Some((guid, route)) = self.pinned.take() else {
            return;
        };
        kernel.unregister_peer(&guid, &route);
        let waiting: Vec<_> = {
            let mut c = self.calls.lock().expect("peer calls poisoned");
            c.gone = true;
            c.waiting.drain().collect()
        };
        for (_, tx) in waiting {
            let _ = tx.send((disconnected(&guid), None));
        }
    }
}

/// python `{"type": "error", "id": frame.get("id", ""), **refusal}`.
async fn answer_error(out: &tokio::sync::mpsc::Sender<Out>, fid: &Value, r: &Value) {
    let _ = out
        .send(Out::Text(fantastic_io_bridge::codec::encode_frame(
            &error_frame(fid, r),
        )))
        .await;
}
