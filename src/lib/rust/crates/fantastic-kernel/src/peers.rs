//! CONNECTED PEER KERNELS — cross-kernel addressing. Python
//! `PeerRoute` + `Kernel.peers` / `register_peer` / `unregister_peer` /
//! `peer_rows` (`kernel/_kernel.py`).
//!
//! An agent on another kernel is addressed `<guid>|<agent id>` and called with an
//! ordinary `send`. When that guid is registered here, the send hands the call to
//! its routes and the peer's reply is the call's ordinary outcome. A TRANSPORT
//! registers the peer it is connected to (the web ws leg, when a page says
//! `hello`) and unregisters it when the connection ends — the transport only
//! carries frames; addressing is the kernel's. A guid that is not here is an
//! unknown target like any other.
//!
//! A SET PER GUID: the tabs of one identity are ONE principal
//! — every tab says hello with the identity's guid, each is a live route of it, a
//! call to `<guid>|x` goes to all of them and settles with the first reply.
//!
//! WATCHING A PEER'S AGENT: `watch(<guid>|<agent>, watcher)` is gated like a
//! send (the watcher's `can_call` must grant `watch` on the qualified id) and
//! asks the peer's routes that carry watches (a peer KERNEL's leg, not a page
//! tab) to stream that agent's events here; each one the transport hands back
//! ([`Kernel::peer_event`]) lands in every watcher's inbox. The subscription is
//! THIS kernel's, per guid: it outlives the connection, and a transport that
//! registers the guid again re-asks for every watched agent
//! ([`Kernel::peer_watched`]). Events emitted while no connection was up are not
//! replayed.

use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use crate::outcome::Blob;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// What one peer call comes back with: the peer's reply, and the raw body AT ITS
/// SLOT of the reply when the reply carried one (the binary channel rides beside
/// the JSON, as it does everywhere in this runtime — python's reply dict simply
/// holds the bytes, at whatever key the peer's handler put them).
pub type PeerReply = (Value, Option<Blob>);

/// The future a route returns for one call.
pub type PeerCallFuture = Pin<Box<dyn Future<Output = PeerReply> + Send>>;

/// `call(agent_id, payload, body)` — deliver ONE call to the peer's `agent_id`,
/// with the raw request body at its payload path when the call carries one. The
/// peer judges what arrives as the transport leg that serves it there — a caller
/// is always local.
type PeerCallFn = dyn Fn(String, Value, Option<Blob>) -> PeerCallFuture + Send + Sync;

/// The future a route returns for one emit — it settles once the frame is handed
/// to the transport (an emit has no reply).
pub type PeerEmitFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// `emit(agent_id, payload, body)` — deliver ONE emit to the peer's `agent_id`;
/// the raw body rides at its payload path.
type PeerEmitFn = dyn Fn(String, Value, Option<Blob>) -> PeerEmitFuture + Send + Sync;

/// `watch(agent_id, on)` — ask the peer to start (`on`) or stop streaming
/// `agent_id`'s events over this route; it settles once the frame is handed to
/// the transport.
type PeerWatchFn = dyn Fn(String, bool) -> PeerEmitFuture + Send + Sync;

/// How a connected PEER KERNEL is reached. `via` names the transport agent
/// carrying it (the root reflect reports it); the call delivers one call and
/// resolves with the peer's reply — or a refusal when the connection ends first.
///
/// Held as an `Arc`: a route is compared by IDENTITY (python `is`), so a
/// connection unregisters its own route and no other.
pub struct PeerRoute {
    /// The transport agent carrying this peer.
    pub via: String,
    call: Box<PeerCallFn>,
    emit: Box<PeerEmitFn>,
    /// `Some` when the peer streams its agents' events back (a kernel's leg);
    /// `None` for a peer that cannot be watched (a page tab).
    watch: Option<Box<PeerWatchFn>>,
}

impl PeerRoute {
    /// A route through `via` whose calls `call` delivers and whose emits `emit`
    /// delivers.
    pub fn new<F, E>(via: impl Into<String>, call: F, emit: E) -> Arc<Self>
    where
        F: Fn(String, Value, Option<Blob>) -> PeerCallFuture + Send + Sync + 'static,
        E: Fn(String, Value, Option<Blob>) -> PeerEmitFuture + Send + Sync + 'static,
    {
        Arc::new(Self {
            via: via.into(),
            call: Box::new(call),
            emit: Box::new(emit),
            watch: None,
        })
    }

    /// [`Self::new`] for a peer whose agents can be WATCHED: `watch` asks it to
    /// start or stop streaming one agent's events over this route.
    pub fn with_watch<F, E, W>(via: impl Into<String>, call: F, emit: E, watch: W) -> Arc<Self>
    where
        F: Fn(String, Value, Option<Blob>) -> PeerCallFuture + Send + Sync + 'static,
        E: Fn(String, Value, Option<Blob>) -> PeerEmitFuture + Send + Sync + 'static,
        W: Fn(String, bool) -> PeerEmitFuture + Send + Sync + 'static,
    {
        Arc::new(Self {
            via: via.into(),
            call: Box::new(call),
            emit: Box::new(emit),
            watch: Some(Box::new(watch)),
        })
    }

    /// Ask the peer to start (`on`) or stop streaming `agent_id`'s events — `None`
    /// when this route carries no watches.
    pub fn watch(&self, agent_id: String, on: bool) -> Option<PeerEmitFuture> {
        self.watch.as_ref().map(|w| w(agent_id, on))
    }

    /// Deliver one emit through this route.
    pub fn emit(&self, agent_id: String, payload: Value, body: Option<Blob>) -> PeerEmitFuture {
        (self.emit)(agent_id, payload, body)
    }

    /// Deliver one call through this route.
    pub fn call(&self, agent_id: String, payload: Value, body: Option<Blob>) -> PeerCallFuture {
        (self.call)(agent_id, payload, body)
    }
}

/// An agent on a CONNECTED PEER KERNEL, as ONE send sees it — python's
/// `RemoteTarget`. Carried on an unregistered [`Agent`] so the send pipeline
/// treats it like any target (a ledger row, a task-table row, a settled outcome),
/// except that its dispatch hands the call to the peer's route instead of a
/// handler here. The peer judges the call as the leg that serves it there.
pub(crate) struct RemoteRef {
    /// The peer kernel's guid.
    pub(crate) guid: String,
    /// The agent's id on that kernel.
    pub(crate) agent_id: String,
    /// The guid's live routes when the send resolved it — every tab of one
    /// identity: the call goes to ALL of them and settles with the first reply.
    pub(crate) routes: Vec<Arc<PeerRoute>>,
}

/// The kernel's peer table — guid → its live routes in registration order,
/// SORTED by guid so `peer_rows` reads in python's `sorted(self.peers.items())`
/// order.
pub(crate) type PeerTable = std::sync::Mutex<BTreeMap<String, Vec<Arc<PeerRoute>>>>;

/// This kernel's watches of peer agents — `(guid, agent id)` → the local
/// watchers. Kept per guid across connections (see the module doc).
pub(crate) type PeerWatchTable = std::sync::Mutex<BTreeMap<(String, String), BTreeSet<String>>>;

impl Kernel {
    fn peer_table(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<Arc<PeerRoute>>>> {
        self.peers.lock().expect("peers lock poisoned")
    }

    /// Make `<guid>|<agent>` reachable through `route` — ADDED to the guid's live
    /// routes: another connection saying hello with a live guid is another tab of
    /// that identity, one principal with the rest. A route already there stays
    /// one.
    pub fn register_peer(&self, guid: &str, route: &Arc<PeerRoute>) {
        let mut table = self.peer_table();
        let routes = table.entry(guid.to_string()).or_default();
        if !routes.iter().any(|r| Arc::ptr_eq(r, route)) {
            routes.push(Arc::clone(route));
        }
    }

    /// Forget `route` — that one connection. The guid stays reachable through its
    /// other live routes, and stops being an address with its last.
    pub fn unregister_peer(&self, guid: &str, route: &Arc<PeerRoute>) {
        let mut table = self.peer_table();
        if let Some(routes) = table.get_mut(guid) {
            routes.retain(|r| !Arc::ptr_eq(r, route));
            if routes.is_empty() {
                table.remove(guid);
            }
        }
    }

    /// The live routes `guid` is connected through — empty when it is not
    /// connected.
    #[must_use]
    pub fn peer_routes(&self, guid: &str) -> Vec<Arc<PeerRoute>> {
        self.peer_table().get(guid).cloned().unwrap_or_default()
    }

    /// The connected peer kernels, for the root reflect: `[{guid, via}]` — one row
    /// per live route, so a guid connected twice shows twice.
    #[must_use]
    pub fn peer_rows(&self) -> Value {
        Value::Array(
            self.peer_table()
                .iter()
                .flat_map(|(g, rs)| rs.iter().map(move |r| json!({"guid": g, "via": r.via})))
                .collect(),
        )
    }

    fn peer_watch_table(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), BTreeSet<String>>> {
        self.peer_watches.lock().expect("peer watches poisoned")
    }

    /// The agents of peer `guid` this kernel watches — what a transport that
    /// (re)connects that guid asks the peer to stream again.
    #[must_use]
    pub fn peer_watched(&self, guid: &str) -> Vec<String> {
        self.peer_watch_table()
            .keys()
            .filter(|(g, _)| g == guid)
            .map(|(_, a)| a.clone())
            .collect()
    }

    /// An event the peer `guid` streamed for its agent `agent_id`: into the inbox
    /// of every local watcher of `<guid>|<agent_id>`, as a local watch mirrors —
    /// and nothing on the state stream (the event happened on the other kernel).
    /// An event nobody here watches is dropped.
    pub fn peer_event(&self, guid: &str, agent_id: &str, item: &crate::inbox::InboxItem) {
        let watchers: Vec<AgentId> = self
            .peer_watch_table()
            .get(&(guid.to_string(), agent_id.to_string()))
            .map(|w| w.iter().map(|id| AgentId::from(id.as_str())).collect())
            .unwrap_or_default();
        for w in watchers {
            if let Some(tx) = self.inboxes.get(&w).map(|e| e.clone()) {
                tx.send(item.clone());
            }
        }
    }

    /// The peer `guid` REFUSED the watch of its agent `agent_id`: the watch is not
    /// registered there, so it is forgotten here and the refusal lands in every
    /// watcher's inbox, in place of the events that will never come. A later
    /// `watch` asks the peer again.
    pub fn peer_watch_refused(
        &self,
        guid: &str,
        agent_id: &str,
        refusal: &crate::inbox::InboxItem,
    ) {
        let watchers = self
            .peer_watch_table()
            .remove(&(guid.to_string(), agent_id.to_string()))
            .unwrap_or_default();
        for w in watchers {
            if let Some(tx) = self
                .inboxes
                .get(&AgentId::from(w.as_str()))
                .map(|e| e.clone())
            {
                tx.send(refusal.clone());
            }
        }
    }

    /// `watch` of an agent on a connected peer (`target` is the remote target one
    /// send would resolve): gated like a send, the watcher must already be a
    /// reader, and the peer must carry watches. The first local watcher of that
    /// agent asks every route of the guid to stream it.
    pub(crate) async fn watch_peer(
        self: &Arc<Self>,
        target: &Arc<Agent>,
        watcher_id: AgentId,
    ) -> Option<Value> {
        let remote = target.remote.as_ref()?;
        if let Some(orig) = crate::send::current_sender() {
            if let Some(denial) = self.gate(&orig, "watch", target) {
                return Some(denial);
            }
        }
        if let Some(r) = self.no_consumer(&watcher_id, &target.id) {
            return Some(r);
        }
        if remote.routes.iter().all(|r| r.watch.is_none()) {
            let mut r = crate::refusal::refusal(
                crate::refusal::UNHANDLED,
                format!(
                    "watch: the peer {} is not a kernel leg — its agents cannot be watched",
                    remote.guid
                ),
            );
            crate::refusal::with(&mut r, "src", json!(target.id.0));
            return Some(r);
        }
        let first = {
            let mut table = self.peer_watch_table();
            let set = table
                .entry((remote.guid.clone(), remote.agent_id.clone()))
                .or_default();
            let first = set.is_empty();
            set.insert(watcher_id.0);
            first
        };
        if first {
            for r in &remote.routes {
                if let Some(f) = r.watch(remote.agent_id.clone(), true) {
                    f.await;
                }
            }
        }
        None
    }

    /// `unwatch` of `<guid>|<agent>`: forget the watcher; the last one asks the
    /// guid's live routes to stop streaming the agent.
    pub(crate) async fn unwatch_peer(&self, qualified: &str, watcher_id: &AgentId) {
        let Some((guid, agent_id)) = qualified.split_once('|') else {
            return;
        };
        let key = (guid.to_string(), agent_id.to_string());
        let last = {
            let mut table = self.peer_watch_table();
            let Some(set) = table.get_mut(&key) else {
                return;
            };
            if !set.remove(&watcher_id.0) {
                return;
            }
            let last = set.is_empty();
            if last {
                table.remove(&key);
            }
            last
        };
        if last {
            for r in self.peer_routes(guid) {
                if let Some(f) = r.watch(agent_id.to_string(), false) {
                    f.await;
                }
            }
        }
    }

    /// This kernel's own guid — the root record's `guid`, once its loader minted
    /// it. `None` before the root has booted.
    pub(crate) fn own_guid(&self) -> Option<String> {
        self.root().and_then(|r| {
            r.meta
                .read()
                .expect("meta lock poisoned")
                .get("guid")
                .and_then(Value::as_str)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
        })
    }

    /// `<guid>|<agent id>`: this kernel's own guid names a LOCAL agent; the guid
    /// of a CONNECTED PEER names an agent there (a remote target the send hands
    /// to that peer's route); anything else names nothing. Python
    /// `Agent._qualified_target`.
    pub(crate) fn qualified_target(&self, target_id: &str) -> Option<Arc<Agent>> {
        let (guid, agent_id) = target_id.split_once('|')?;
        if guid.is_empty() || agent_id.is_empty() || agent_id.contains('|') {
            return None;
        }
        if self.own_guid().as_deref() == Some(guid) {
            return if agent_id == "kernel" {
                self.root()
            } else {
                self.agents
                    .get(&AgentId::from(agent_id))
                    .map(|e| Arc::clone(&e))
            };
        }
        let routes = self.peer_routes(guid);
        if routes.is_empty() {
            return None;
        }
        Some(Agent::remote(
            AgentId::from(target_id),
            RemoteRef {
                guid: guid.to_string(),
                agent_id: agent_id.to_string(),
                routes,
            },
        ))
    }
}

/// The dispatch of a call to a remote target: hand it to EVERY live route of the
/// peer (each tab of one identity); the first reply settles it and the other
/// calls are dropped. Python `RemoteTarget._dispatch_task`.
pub(crate) async fn call_peer(remote: &RemoteRef, payload: Value, body: Option<Blob>) -> PeerReply {
    let calls = remote
        .routes
        .iter()
        .map(|r| r.call(remote.agent_id.clone(), payload.clone(), body.clone()));
    // The losers are DROPPED with `select_all`'s rest — their routes forget them.
    // TODO: first-reply-wins hides tab DESYNC — if the tabs of one
    // identity diverge, the other replies are dropped unseen. Must be solved
    // (later).
    let (first, _, _rest) = futures_util::future::select_all(calls).await;
    first
}

/// An emit to a remote target: hand it to EVERY live route of the peer (each tab
/// of one identity). GATED LIKE A SEND when an agent emits: its `can_call` must
/// grant `emit` on `<guid>|<agent>` (egress; the peer gates its own agents). A
/// refused emit is dropped — an emit has no reply. Python `Agent._emit_remote`.
pub(crate) async fn emit_peer(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    item: crate::inbox::InboxItem,
) {
    let Some(remote) = target.remote.as_ref() else {
        return;
    };
    if let Some(sender) = crate::send::current_sender() {
        if kernel.gate(&sender, "emit", target).is_some() {
            return;
        }
    }
    // TODO: DESYNC between the tabs of one identity is envisioned
    // and unsolved — each tab gets this emit, nothing reconciles tabs that have
    // diverged. Think later.
    for r in &remote.routes {
        r.emit(
            remote.agent_id.clone(),
            item.payload.clone(),
            item.body.clone(),
        )
        .await;
    }
}

/// A remote target's meta — python's `{"callable_by": {}}`: it accepts nothing
/// HERE. Whether it accepts a call is its own kernel's decision.
pub(crate) fn remote_meta() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("callable_by".to_string(), json!({}));
    m
}
