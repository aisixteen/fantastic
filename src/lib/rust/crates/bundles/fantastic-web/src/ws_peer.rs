//! THE PEER HALF of the WS leg: calls the host makes INTO the page — python
//! `web_ws._proxy.run`'s `_call_page` / `_on_hello` / `_on_answer` and the
//! disconnect settle in its `finally`.
//!
//! A page's kernel says who it is (`hello {guid}`); from then on the host kernel
//! can address its agents as `<guid>|<agent>` (kernel `peers`), and every such
//! call rides THIS socket as an outbound `call` frame, answered by an inbound
//! `reply`/`error` with the same id. This leg only carries the frames — the host
//! kernel routes and gates, the page kernel gates its own agents. One guid per
//! connection, pinned by the first hello; many connections per guid (every tab
//! of one identity). A host EMIT to `<guid>|<agent>` rides every tab of the guid
//! as an outbound `emit` frame, and a reply this leg sends one tab is MIRRORED to
//! the guid's other tabs as a `reply_mirror` frame.
//!
//! A KERNEL'S LEG says `hello {guid, leg}` (a `ws_bridge` dialing in): the same
//! address, plus the two-way parts a page has no use for. The host may WATCH that
//! kernel's agents — a host `watch` of `<guid>|<agent>` rides this socket as an
//! outbound `{type:"watch"|"unwatch", src}` frame, and the leg streams the agent's
//! events back as inbound `{type:"event", src, payload}` frames, handed to the
//! host kernel's watchers ([`Kernel::peer_event`]). ONE connection per kernel
//! guid: a newer hello for the same guid REPLACES the older connection (its
//! waiting calls settle `bridge_down`, the socket is closed), and the host's
//! watches of that guid are asked for again on the new one. Kernel legs join no
//! tabs and get no reply mirror.

use super::{error_frame, gate_leg, send_envelope, Out, INBOUND_PAYLOAD_KEY, REPLY_KEY};
use fantastic_io_bridge::authorizer::Decision;
use fantastic_kernel::refusal::{bad, refusal, TRANSPORT_ERROR};
use fantastic_kernel::{AgentId, Kernel, PeerReply, PeerRoute};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc::WeakSender;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// The live connections of each `(kernel, hello guid)`, by connection (its
/// client id) — the tabs of one identity, for the reply mirror. Python `_TABS`,
/// keyed `(id(kernel.ctx), guid)`.
type TabMap = BTreeMap<(usize, String), Vec<(String, WeakSender<Out>)>>;
static TABS: Mutex<TabMap> = Mutex::new(BTreeMap::new());

/// The ONE live connection of each `(kernel, hello guid)` a kernel's leg said
/// hello on — the newest wins.
type LegMap = BTreeMap<(usize, String), LegConn>;
static LEGS: Mutex<LegMap> = Mutex::new(BTreeMap::new());

/// What a newer hello needs to retire an older kernel-leg connection.
struct LegConn {
    /// The connection's client id.
    me: String,
    route: Arc<PeerRoute>,
    calls: Arc<Mutex<Calls>>,
    out: WeakSender<Out>,
}

/// This connection's peer: the pinned guid, its route, and the calls in flight.
pub(super) struct PeerLink {
    /// The guid the first accepted hello pinned, and the route registered for it.
    pinned: Option<(String, Arc<PeerRoute>)>,
    /// The calls waiting for an answer, by frame id (`h-<nonce>-<n>`).
    calls: Arc<Mutex<Calls>>,
    /// The kernel's key in [`TABS`]; this connection joins its guid's tabs at
    /// hello.
    kernel: usize,
    /// This connection's client id.
    me: String,
    /// The pinned guid, readable by this connection's call tasks.
    guid: Arc<OnceLock<String>>,
    /// The dialing leg's id when a KERNEL said hello (`hello {guid, leg}`);
    /// `None` for a page.
    leg: Option<String>,
}

/// What a `call` task of this connection needs to mirror its reply.
pub(super) struct Mirror {
    kernel: usize,
    me: String,
    guid: Arc<OnceLock<String>>,
}

impl Mirror {
    /// The OTHER live connections of this connection's hello guid — none before
    /// hello.
    pub(super) fn siblings(&self) -> Vec<tokio::sync::mpsc::Sender<Out>> {
        let Some(guid) = self.guid.get() else {
            return Vec::new();
        };
        TABS.lock()
            .expect("tabs poisoned")
            .get(&(self.kernel, guid.clone()))
            .map(|v| {
                v.iter()
                    .filter(|(c, _)| *c != self.me)
                    .filter_map(|(_, w)| w.upgrade())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// REPLY MIRRORING: the reply this leg sent one tab — an outcome or a teaching
/// denial — to each other tab of its identity on this kernel as `{type:"reply_mirror", id, data}` — the page emits it on its
/// bridge's own inbox (it resolves no call there). The body rides at its reply
/// slot, as on the reply.
// TODO: DESYNC between the tabs of one identity is envisioned and
// unsolved — a mirror informs the other tabs, nothing reconciles tabs that have
// diverged. Think later.
pub(super) async fn mirror_reply(
    siblings: Vec<tokio::sync::mpsc::Sender<Out>>,
    fid: &Value,
    data: &Value,
    body: Option<&fantastic_kernel::Blob>,
) {
    for tx in siblings {
        let f = json!({"type": "reply_mirror", "id": fid, "data": data});
        send_envelope(&tx, f, body.cloned(), REPLY_KEY).await;
    }
}

/// The outbound calls of one connection.
struct Calls {
    /// `h-<12 hex>`, minted per CONNECTION: the frame id NAMES the execution on
    /// the peer, so a reconnected peer kernel must never see an id an earlier
    /// connection already used (the counter restarts with every connection) —
    /// it would join that settled execution instead of running.
    prefix: String,
    /// The last frame id handed out — ids are `<prefix>-1`, `<prefix>-2`, … per
    /// connection.
    seq: u64,
    /// Frame id → the waiting call; it is answered with the reply and the
    /// reply frame's `secret` marks.
    waiting: HashMap<String, oneshot::Sender<(PeerReply, Vec<String>)>>,
    /// Set once the socket is gone: a call that reaches the route after that
    /// settles at once instead of waiting on a peer that can no longer answer.
    gone: bool,
    /// Who answers on this connection, for the ending of a call it never
    /// answers: `the page <guid>`, or a kernel's leg.
    peer: Peer,
}

/// Who said hello on a connection.
#[derive(Default, Clone)]
enum Peer {
    /// A page (no `leg`) — or nobody yet.
    #[default]
    Page,
    /// A kernel's dialing leg (`hello {guid, leg}`).
    Kernel(String),
}

/// The one ending for a call the peer will never answer. A kernel's leg ends
/// it `bridge_down` — the link that carried it dropped (or a newer hello
/// replaced it); the caller may call again once the leg redials.
fn disconnected(peer: &Peer, guid: &str) -> Value {
    match peer {
        Peer::Page => refusal(
            TRANSPORT_ERROR,
            format!("the page {guid} disconnected before answering"),
        ),
        Peer::Kernel(leg) => refusal(
            TRANSPORT_ERROR,
            format!("bridge_down: the kernel {guid} (leg {leg}) disconnected before answering"),
        ),
    }
}

/// Settle every call still waiting on `calls` NOW with the disconnect, and
/// refuse the ones still to come.
fn settle_gone(calls: &Mutex<Calls>, guid: &str) {
    let (waiting, peer): (Vec<_>, Peer) = {
        let mut c = calls.lock().expect("peer calls poisoned");
        c.gone = true;
        (c.waiting.drain().collect(), c.peer.clone())
    };
    for (_, tx) in waiting {
        let _ = tx.send(((disconnected(&peer, guid), None), Vec::new()));
    }
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

/// ONE call into the page: an outbound `{type:"call", id, target, payload}`
/// frame, resolved by the inbound answer carrying the same id. The page judges
/// it as the leg that serves it there. A body rides the binary channel at
/// `payload.<its path>` — where python's payload dict holds it and where the
/// page's decoder puts it back.
async fn call_page(
    calls: Arc<Mutex<Calls>>,
    out: WeakSender<Out>,
    guid: String,
    agent_id: String,
    mut payload: Value,
    body: Option<fantastic_kernel::Blob>,
) -> PeerReply {
    let (fid, rx, peer) = {
        let mut c = calls.lock().expect("peer calls poisoned");
        if c.gone {
            return (disconnected(&c.peer, &guid), None);
        }
        c.seq += 1;
        let fid = format!("{}-{}", c.prefix, c.seq);
        let (tx, rx) = oneshot::channel();
        c.waiting.insert(fid.clone(), tx);
        (fid, rx, c.peer.clone())
    };
    let _forget = Forget {
        calls: Arc::clone(&calls),
        fid: fid.clone(),
    };
    // The route holds the sink WEAKLY: a call still in flight must not keep a
    // closed socket's writer alive.
    let Some(out) = out.upgrade() else {
        return (disconnected(&peer, &guid), None);
    };
    // the slot python's dict holds the bytes in: `null` on the head, in place
    // (the kernel admitted the path at launch)
    if let Some(b) = &body {
        let _ = b.place(&mut payload, Value::Null);
    }
    let envelope = json!({
        "type": "call",
        "id": fid,
        "target": agent_id,
        "payload": payload,
    });
    send_envelope(&out, envelope, body, INBOUND_PAYLOAD_KEY).await;
    drop(out);
    // The sender side is dropped only by the disconnect settle, which answers
    // first — a closed channel still means the peer is gone.
    let (reply, marks) = rx
        .await
        .unwrap_or_else(|_| ((disconnected(&peer, &guid), None), Vec::new()));
    // The reply frame's own secret marks, rooted at this reply: this kernel
    // records the call's reply as `[secret]` there (this runs inside the remote
    // target's dispatch); the caller gets it whole.
    fantastic_kernel::note_reply_secret(marks);
    reply
}

/// Ask a kernel's leg to start (`on`) or stop streaming its agent `agent_id`:
/// an outbound `{type:"watch"|"unwatch", src}` frame. A gone socket drops it —
/// the host's watch stays, and is asked for again when the guid reconnects.
async fn watch_leg(out: WeakSender<Out>, agent_id: String, on: bool) {
    let Some(out) = out.upgrade() else {
        return;
    };
    let kind = if on { "watch" } else { "unwatch" };
    let f = json!({"type": kind, "src": agent_id});
    let _ = out
        .send(Out::Text(fantastic_io_bridge::codec::encode_frame(&f)))
        .await;
}

/// ONE emit into the page: an outbound `{type:"emit", target, payload}` frame.
/// The body rides the binary channel at `payload.<its path>`, as on a call. No
/// answer: a gone socket drops it.
async fn emit_page(
    out: WeakSender<Out>,
    agent_id: String,
    mut payload: Value,
    body: Option<fantastic_kernel::Blob>,
) {
    let Some(out) = out.upgrade() else {
        return;
    };
    if let Some(b) = &body {
        let _ = b.place(&mut payload, Value::Null);
    }
    let envelope = json!({"type": "emit", "target": agent_id, "payload": payload});
    send_envelope(&out, envelope, body, INBOUND_PAYLOAD_KEY).await;
}

impl PeerLink {
    /// The guid this connection's hello pinned, if it said one.
    pub(super) fn guid(&self) -> Option<String> {
        self.pinned.as_ref().map(|(g, _)| g.clone())
    }

    /// No peer yet: the connection `me` to `kernel`.
    pub(super) fn new(kernel: &Arc<Kernel>, me: String) -> Self {
        let mut nonce = [0u8; 6];
        fantastic_kernel::lifecycle::fill_random(&mut nonce);
        let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        Self {
            pinned: None,
            calls: Arc::new(Mutex::new(Calls {
                prefix: format!("h-{hex}"),
                seq: 0,
                waiting: HashMap::new(),
                gone: false,
                peer: Peer::Page,
            })),
            kernel: Arc::as_ptr(kernel) as usize,
            me,
            guid: Arc::default(),
            leg: None,
        }
    }

    /// The handle a `call` task mirrors its reply through.
    pub(super) fn mirror(&self) -> Mirror {
        Mirror {
            kernel: self.kernel,
            me: self.me.clone(),
            guid: Arc::clone(&self.guid),
        }
    }

    /// `hello {guid, leg?}` — make this connection the peer kernel's address.
    /// Python `_on_hello`: the guid (and a `leg`, when given) is checked first (a
    /// malformed one is answered even on a sealed leg), then the leg's gate (a
    /// sealed leg carries no peer, and says nothing — the calls it refuses
    /// teach), then the pin. A PAGE's guid already live on another connection is
    /// another tab of that identity: this route JOINS it (one principal, kernel
    /// `register_peer`). A KERNEL's leg (`leg` given) REPLACES the guid's older
    /// leg connection instead, and the host's watches of that guid are asked for
    /// again on this one.
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
        let dialer = match frame.get("leg") {
            None => None,
            Some(Value::String(l)) if !l.is_empty() => Some(l.clone()),
            Some(_) => {
                let r = bad(
                    "hello",
                    "leg",
                    "the dialing kernel leg's id, a non-empty string",
                );
                answer_error(out, fid, &r).await;
                return;
            }
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
        let call = {
            let calls = Arc::clone(&self.calls);
            let weak = out.downgrade();
            let g = guid.to_string();
            move |agent_id, payload, body| -> fantastic_kernel::PeerCallFuture {
                Box::pin(call_page(
                    Arc::clone(&calls),
                    weak.clone(),
                    g.clone(),
                    agent_id,
                    payload,
                    body,
                ))
            }
        };
        let emit = {
            let weak_emit = out.downgrade();
            move |agent_id, payload, body| -> fantastic_kernel::PeerEmitFuture {
                Box::pin(emit_page(weak_emit.clone(), agent_id, payload, body))
            }
        };
        let Some(dialer) = dialer else {
            let route = PeerRoute::new(leg.0.clone(), call, emit);
            kernel.register_peer(guid, &route);
            TABS.lock()
                .expect("tabs poisoned")
                .entry((self.kernel, guid.to_string()))
                .or_default()
                .push((self.me.clone(), out.downgrade()));
            let _ = self.guid.set(guid.to_string());
            self.pinned = Some((guid.to_string(), route));
            return;
        };
        let weak_watch = out.downgrade();
        let route = PeerRoute::with_watch(leg.0.clone(), call, emit, move |agent_id, on| {
            Box::pin(watch_leg(weak_watch.clone(), agent_id, on))
        });
        self.calls.lock().expect("peer calls poisoned").peer = Peer::Kernel(dialer.clone());
        // THE NEWEST CONNECTION WINS: the older one stops being the address
        // before this one becomes it, so no call is split across the two.
        let older = LEGS.lock().expect("legs poisoned").insert(
            (self.kernel, guid.to_string()),
            LegConn {
                me: self.me.clone(),
                route: Arc::clone(&route),
                calls: Arc::clone(&self.calls),
                out: out.downgrade(),
            },
        );
        if let Some(old) = older.filter(|o| o.me != self.me) {
            kernel.unregister_peer(guid, &old.route);
            settle_gone(&old.calls, guid);
            if let Some(tx) = old.out.upgrade() {
                let _ = tx
                    .send(Out::Close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: format!("replaced by a newer hello for {guid}").into(),
                    })))
                    .await;
            }
        }
        kernel.register_peer(guid, &route);
        let _ = self.guid.set(guid.to_string());
        self.leg = Some(dialer);
        self.pinned = Some((guid.to_string(), route));
        // the host's watches of this guid resume on the new connection
        for agent_id in kernel.peer_watched(guid) {
            watch_leg(out.downgrade(), agent_id, true).await;
        }
    }

    /// An inbound `{type:"event", src, payload}` from a kernel's leg: an event of
    /// its agent `src` that this host watches — into the host's watchers of
    /// `<guid>|<src>`. A page's events, or one before hello, are dropped. The
    /// body (a binary frame) rides at its path inside `payload`.
    pub(super) fn event(
        &self,
        kernel: &Kernel,
        frame: &Value,
        body: Option<fantastic_kernel::Blob>,
    ) {
        if self.leg.is_none() {
            return;
        }
        let Some((guid, _)) = &self.pinned else {
            return;
        };
        let Some(src) = frame
            .get("src")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let Some(payload) = frame.get("payload").filter(|p| p.is_object()) else {
            return;
        };
        let item = match body {
            Some(b) => fantastic_kernel::inbox::InboxItem::binary(payload.clone(), b),
            None => fantastic_kernel::inbox::InboxItem::text(payload.clone()),
        };
        kernel.peer_event(guid, src, &item);
    }

    /// An inbound `{type:"error", src, …refusal}` from a kernel's leg: it refused
    /// the host's `watch` of its agent `src` (a call's answer carries an `id` and
    /// no `src`). The refusal reaches the host's watchers of `<guid>|<src>` as the
    /// frame's own keys, and the watch is forgotten.
    fn refused_watch(&self, kernel: &Kernel, frame: &Value) -> bool {
        if self.leg.is_none() || frame.get("type").and_then(Value::as_str) != Some("error") {
            return false;
        }
        let Some(src) = frame
            .get("src")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return false;
        };
        let Some((guid, _)) = &self.pinned else {
            return true;
        };
        let mut refusal = frame.clone();
        if let Some(m) = refusal.as_object_mut() {
            m.remove("id");
        }
        kernel.peer_watch_refused(
            guid,
            src,
            &fantastic_kernel::inbox::InboxItem::text(refusal),
        );
        true
    }

    /// An inbound `reply`/`error` answers one of OUR outbound calls — its `data`
    /// is that call's outcome (a page-side refusal included), a non-object
    /// wrapped as `{result}`. An id we are not waiting on is ignored. Python
    /// `_on_answer`.
    ///
    /// `body` is the frame's raw body at its envelope path (`["data", …]`): the
    /// reply holds it at the rest of that path — under `result` when `data` was
    /// not an object and got wrapped, as python's dict holds it.
    pub(super) fn answer(
        &self,
        kernel: &Kernel,
        frame: &Value,
        body: Option<fantastic_kernel::Blob>,
    ) {
        if self.refused_watch(kernel, frame) {
            return;
        }
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
        // the reply's own secret paths (malformed ⇒ none; the reply still lands)
        let marks = fantastic_kernel::secret::check(frame.get("secret")).unwrap_or_default();
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
        let _ = tx.send(((data, body), marks));
    }

    /// The page is gone: it is no longer an address, and every call still
    /// waiting on it settles NOW with the reason — never a hang, never a retry.
    pub(super) fn close(&mut self, kernel: &Kernel) {
        let Some((guid, route)) = self.pinned.take() else {
            return;
        };
        kernel.unregister_peer(&guid, &route);
        let key = (self.kernel, guid.clone());
        if self.leg.is_some() {
            // only this connection's own entry — a newer hello's stays
            let mut legs = LEGS.lock().expect("legs poisoned");
            if legs.get(&key).is_some_and(|l| l.me == self.me) {
                legs.remove(&key);
            }
        } else {
            let mut tabs = TABS.lock().expect("tabs poisoned");
            if let Some(v) = tabs.get_mut(&key) {
                v.retain(|(c, _)| *c != self.me);
                if v.is_empty() {
                    tabs.remove(&key);
                }
            }
        }
        settle_gone(&self.calls, &guid);
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
